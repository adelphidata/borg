//! Terminal notification when a long agent turn finishes.
//!
//! A user who backgrounds the terminal during a multi-minute turn gets no
//! signal when it ends. This module builds the escape sequence that asks
//! the host terminal to raise a desktop notification — OSC 9 for the
//! iTerm2/ConEmu family, OSC 777 for terminals that implement the
//! `notify` extension — and falls back to a plain BEL everywhere else,
//! which terminals map to a visual/audible bell or a badge.
//!
//! Every function here is pure. The only terminal I/O is [`emit`], which the
//! event loop calls with the live backend. None of the sequences touch mouse
//! tracking modes; see the guard in `tui/mod.rs`.

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

/// Turns shorter than this never notify: a quick reply is already on screen
/// before the user could have looked away, and a ping per short exchange is
/// noise.
pub const NOTIFY_MIN_TURN_SECS: u64 = 5;

/// Which notification protocol the host terminal understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    /// `ESC ] 9 ; <body> BEL` — iTerm2, ConEmu, Windows Terminal.
    Osc9,
    /// `ESC ] 777 ; notify ; <title> ; <body> BEL` — rxvt-unicode, WezTerm,
    /// Ghostty, foot.
    Osc777,
    /// Plain `BEL`. Terminals without a notification OSC still surface a
    /// bell (badge, flash, or sound), and multiplexers turn it into a
    /// window flag.
    Bell,
}

/// Pick the notification protocol from the process environment.
///
/// Takes the map rather than reading `std::env` so the table is testable.
/// A multiplexer wins over the outer terminal: tmux/screen swallow unknown
/// OSC sequences unless passthrough is configured, and a bell still reaches
/// the user as a window flag.
pub fn detect_notify_kind(env: &HashMap<String, String>) -> NotifyKind {
    let get = |key: &str| env.get(key).map(String::as_str).unwrap_or("");
    if !get("TMUX").is_empty() || !get("STY").is_empty() {
        return NotifyKind::Bell;
    }
    let term_program = get("TERM_PROGRAM").to_ascii_lowercase();
    let term = get("TERM").to_ascii_lowercase();
    if term_program == "iterm.app" || !get("ConEmuANSI").is_empty() || !get("WT_SESSION").is_empty()
    {
        return NotifyKind::Osc9;
    }
    if matches!(term_program.as_str(), "wezterm" | "ghostty" | "foot")
        || term.starts_with("rxvt")
        || term.starts_with("foot")
    {
        return NotifyKind::Osc777;
    }
    NotifyKind::Bell
}

/// [`detect_notify_kind`] over the live process environment.
pub fn detect_from_process_env() -> NotifyKind {
    detect_notify_kind(&std::env::vars().collect())
}

/// Remove every control character (C0, DEL, C1) so embedded text cannot
/// terminate or corrupt the enclosing sequence. `ESC` and `BEL` are the
/// dangerous ones — both end an OSC string early — but `\n`, `\r` and C1
/// controls are dropped too since terminals render none of them in a
/// notification.
fn sanitize(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Build the escape sequence for `kind`.
///
/// Returns `None` when an OSC kind would carry no text at all after
/// sanitizing — an empty notification is worse than silence. `Bell` always
/// yields the single BEL byte. Title and body are sanitized; `;` in the
/// title becomes `,` so it cannot shift the OSC 777 field layout.
pub fn notification_sequence(kind: NotifyKind, title: &str, body: &str) -> Option<String> {
    let title = sanitize(title).replace(';', ",");
    let body = sanitize(body);
    match kind {
        NotifyKind::Bell => Some("\x07".to_string()),
        NotifyKind::Osc9 => {
            // OSC 9 has no title field; fold the title into the body.
            let text = match (title.is_empty(), body.is_empty()) {
                (true, true) => return None,
                (false, true) => title,
                (true, false) => body,
                (false, false) => format!("{title}: {body}"),
            };
            Some(format!("\x1b]9;{text}\x07"))
        }
        NotifyKind::Osc777 => {
            if title.is_empty() && body.is_empty() {
                return None;
            }
            Some(format!("\x1b]777;notify;{title};{body}\x07"))
        }
    }
}

/// Gate: notify only when the user opted in and the turn ran long enough
/// that they plausibly looked away.
pub fn should_notify(enabled: bool, elapsed: Duration) -> bool {
    enabled && elapsed >= Duration::from_secs(NOTIFY_MIN_TURN_SECS)
}

/// Write `sequence` to the terminal and flush.
///
/// Errors are dropped on purpose: a notification is terminal cosmetics, and
/// a write failure here (closed pipe, detached TTY) is about to surface from
/// the next frame draw anyway, where it is handled.
pub fn emit(out: &mut impl Write, sequence: &str) {
    let _ = out
        .write_all(sequence.as_bytes())
        .and_then(|()| out.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn detection_table() {
        let cases: &[(&[(&str, &str)], NotifyKind)] = &[
            (&[], NotifyKind::Bell),
            (&[("TERM_PROGRAM", "iTerm.app")], NotifyKind::Osc9),
            (&[("ConEmuANSI", "ON")], NotifyKind::Osc9),
            (&[("WT_SESSION", "abc")], NotifyKind::Osc9),
            (&[("TERM_PROGRAM", "WezTerm")], NotifyKind::Osc777),
            (&[("TERM_PROGRAM", "ghostty")], NotifyKind::Osc777),
            (&[("TERM", "rxvt-unicode-256color")], NotifyKind::Osc777),
            (&[("TERM", "foot")], NotifyKind::Osc777),
            (&[("TERM_PROGRAM", "Apple_Terminal")], NotifyKind::Bell),
            (&[("TERM_PROGRAM", "vscode")], NotifyKind::Bell),
            // A multiplexer swallows OSC unless passthrough is on; bell wins.
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TMUX", "/tmp/tmux-1/default,1,0"),
                ],
                NotifyKind::Bell,
            ),
            (
                &[("TERM_PROGRAM", "WezTerm"), ("STY", "1234.pts-0")],
                NotifyKind::Bell,
            ),
        ];
        for (pairs, expected) in cases {
            assert_eq!(detect_notify_kind(&env(pairs)), *expected, "env={pairs:?}");
        }
    }

    #[test]
    fn sequence_shape_per_kind() {
        assert_eq!(
            notification_sequence(NotifyKind::Osc9, "Borg", "Finished in 12s").as_deref(),
            Some("\x1b]9;Borg: Finished in 12s\x07")
        );
        assert_eq!(
            notification_sequence(NotifyKind::Osc777, "Borg", "Finished in 12s").as_deref(),
            Some("\x1b]777;notify;Borg;Finished in 12s\x07")
        );
        assert_eq!(
            notification_sequence(NotifyKind::Bell, "Borg", "Finished in 12s").as_deref(),
            Some("\x07")
        );
        // OSC 9 with no title does not emit a dangling separator.
        assert_eq!(
            notification_sequence(NotifyKind::Osc9, "", "done").as_deref(),
            Some("\x1b]9;done\x07")
        );
    }

    #[test]
    fn empty_text_yields_no_osc_but_still_bells() {
        assert_eq!(notification_sequence(NotifyKind::Osc9, "", ""), None);
        assert_eq!(notification_sequence(NotifyKind::Osc777, "", ""), None);
        assert_eq!(
            notification_sequence(NotifyKind::Bell, "", "").as_deref(),
            Some("\x07")
        );
    }

    #[test]
    fn control_characters_cannot_break_out_of_the_sequence() {
        // ESC/BEL/ST inside the body would terminate the OSC early and let
        // the remainder run as raw terminal input.
        let hostile = "ok\x07\x1b]0;pwned\x07\x1b\\\r\n\u{9c}tail";
        for kind in [NotifyKind::Osc9, NotifyKind::Osc777] {
            let seq = notification_sequence(kind, "T\x1bitle", hostile).expect("some");
            // Exactly one OSC introducer and one BEL terminator: the
            // sequence's own. Nothing in between may be a control byte.
            let payload = seq
                .strip_prefix("\x1b]")
                .and_then(|s| s.strip_suffix('\x07'))
                .unwrap_or_else(|| panic!("{kind:?}: bad framing: {seq:?}"));
            assert!(
                !payload.chars().any(char::is_control),
                "{kind:?}: control byte survived: {seq:?}"
            );
            // The printable residue of the injected sequence stays inert text.
            assert!(payload.ends_with("ok]0;pwned\\tail"), "{kind:?}: {seq:?}");
            assert!(payload.contains("Title"), "{kind:?}: {seq:?}");
        }
    }

    #[test]
    fn semicolon_in_title_does_not_shift_osc777_fields() {
        let seq = notification_sequence(NotifyKind::Osc777, "a;b", "body").unwrap();
        let fields: Vec<&str> = seq.trim_end_matches('\x07').split(';').collect();
        assert_eq!(fields, ["\x1b]777", "notify", "a,b", "body"]);
    }

    #[test]
    fn threshold_gating_table() {
        let cases: &[(bool, u64, bool)] = &[
            (true, 0, false),
            (true, NOTIFY_MIN_TURN_SECS - 1, false),
            (true, NOTIFY_MIN_TURN_SECS, true),
            (true, 600, true),
            (false, 600, false),
            (false, 0, false),
        ];
        for (enabled, secs, expected) in cases {
            assert_eq!(
                should_notify(*enabled, Duration::from_secs(*secs)),
                *expected,
                "enabled={enabled} secs={secs}"
            );
        }
    }

    #[test]
    fn emit_writes_and_flushes_sequence() {
        let mut out: Vec<u8> = Vec::new();
        emit(&mut out, "\x1b]9;x\x07");
        assert_eq!(out, b"\x1b]9;x\x07");
    }

    #[test]
    fn sequences_never_touch_mouse_tracking_modes() {
        // Belt-and-braces with the guard in tui/mod.rs: nothing this module
        // emits may enable a mouse mode, or text selection regresses.
        for kind in [NotifyKind::Osc9, NotifyKind::Osc777, NotifyKind::Bell] {
            let seq = notification_sequence(kind, "t", "b").unwrap();
            assert!(!seq.contains("\x1b["), "{kind:?} emitted a CSI: {seq:?}");
        }
    }
}
