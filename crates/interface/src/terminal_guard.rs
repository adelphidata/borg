//! Shared RAII guard for one-shot TUI surfaces (onboarding, migrate, update prompt).
//!
//! These surfaces enable raw mode + alt-screen manually (without the full
//! `tui_lifecycle::set_modes()` setup), so their teardown is the matching
//! lighter pair: `disable_raw_mode()` + `LeaveAlternateScreen`. The main
//! `tui::run` loop has its own guard that inverts `tui_lifecycle::set_modes()`
//! and is intentionally different.

use std::io::stdout;

use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use crossterm::ExecutableCommand;

pub(crate) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = stdout().execute(LeaveAlternateScreen);
    }
}
