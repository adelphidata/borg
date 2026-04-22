//! Client-side expansion of `@path` mentions in user input.
//!
//! When a user types `@foo.rs` or `@some/dir/` into the TUI composer,
//! `expand_mentions` resolves each token against the current working
//! directory, reads the file (or lists the directory), and returns both:
//!
//! 1. An *augmented text* string containing `<attached_file>` /
//!    `<attached_dir>` blocks appended to the user's message — this is what
//!    the LLM sees. The LLM can answer in a single turn instead of deciding
//!    to call `read_file` / `list_dir`.
//! 2. A vector of [`MentionCard`] records the TUI renders as one-line rows
//!    (`Read foo.rs (42 lines)` / `Listed directory foo/`).
//!
//! Matches opencode's per-file cap of 50 KB; on overflow, a truncation
//! marker is appended to the attached content so the LLM knows to ask for
//! specific byte ranges. No aggregate cap — the per-file cap plus the
//! system prompt token budget are the bounds.
//!
//! Paths matching `[security] blocked_paths` are never read; they produce
//! a `Skipped(BlockedPath)` card and the raw `@token` is left in the user
//! text so the LLM can still see what was requested.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::tool_handlers::filesystem::{handle_list_dir, is_blocked_path};

/// Opencode `read.ts` uses the same constant (`MAX_BYTES = 50 * 1024`).
/// Files larger than this are truncated at a UTF-8 boundary and a marker
/// line is appended.
const MAX_FILE_BYTES: usize = 50 * 1024;

/// Characters we strip from the tail of a matched `@token` so trailing
/// sentence punctuation doesn't become part of the path.
///
/// We keep `/` (meaningful for dirs) and `.` inside the token (e.g.
/// `foo.rs`), only stripping `.` when it's at the very end (`see @foo.`).
const TRAILING_PUNCT: &[char] = &['.', ',', '?', '!', ':', ';', ')', ']', '}', '"', '\''];

/// Result of expanding `@mentions` in a user message.
#[derive(Debug, Clone)]
pub struct MentionExpansion {
    /// Text to send to the LLM. Original input with `<attached_file>` /
    /// `<attached_dir>` blocks appended for each successfully resolved
    /// mention. Unresolved mentions (missing/blocked) are left in the text
    /// as-is.
    pub augmented_text: String,
    /// One card per `@token` matched. The TUI renders these above the
    /// assistant's response; they are not sent to the LLM (the content
    /// goes via `augmented_text` instead).
    pub cards: Vec<MentionCard>,
}

/// One-line record for each matched `@token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCard {
    /// The exact substring matched (including the leading `@`), used for
    /// diagnostics and tests.
    pub raw_token: String,
    /// What happened.
    pub kind: MentionCardKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionCardKind {
    /// A file was read. `lines` is the full line count of the file,
    /// even if the attached content was truncated (so the card label
    /// remains accurate).
    File { display: String, lines: usize },
    /// A directory was listed (depth 1).
    Dir { display: String },
    /// The path exists but is denied by `[security] blocked_paths`, or
    /// doesn't exist, or couldn't be read. The raw `@token` is left in
    /// the user text unchanged.
    Skipped { display: String, reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    BlockedPath,
    NotFound,
    ReadError(String),
}

impl MentionCard {
    /// Formatted label matching the reference screenshot:
    /// `Read CLAUDE.md (275 lines)` / `Listed directory foo/` /
    /// `Skipped @foo (not found)`.
    pub fn label(&self) -> String {
        match &self.kind {
            MentionCardKind::File { display, lines } => {
                let noun = if *lines == 1 { "line" } else { "lines" };
                format!("Read {display} ({lines} {noun})")
            }
            MentionCardKind::Dir { display } => format!("Listed directory {display}"),
            MentionCardKind::Skipped { display, reason } => {
                let r = match reason {
                    SkipReason::BlockedPath => "blocked path".to_string(),
                    SkipReason::NotFound => "not found".to_string(),
                    SkipReason::ReadError(e) => format!("error: {e}"),
                };
                format!("Skipped @{display} ({r})")
            }
        }
    }
}

/// Parse `@path` tokens from `text` and return their byte ranges and
/// cleaned path strings.
///
/// A match requires `@` at position 0 or immediately after a whitespace
/// char — this rules out emails (`foo@bar.com`). The path runs until the
/// next whitespace; trailing sentence punctuation is stripped (see
/// [`TRAILING_PUNCT`]).
fn find_mentions(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let preceded_ok = i == 0
                || text[..i]
                    .chars()
                    .next_back()
                    .map(char::is_whitespace)
                    .unwrap_or(false);
            if preceded_ok {
                // Find end-of-token (first whitespace or EOF).
                let start = i;
                let mut j = i + 1;
                while let Some(ch) = text[j..].chars().next() {
                    if ch.is_whitespace() {
                        break;
                    }
                    j += ch.len_utf8();
                }
                let tok = &text[i + 1..j];
                // Strip trailing punctuation (but keep `/` for dirs).
                let mut end = j;
                let mut tok_end = tok.len();
                while let Some(last_char) = tok[..tok_end].chars().next_back() {
                    if TRAILING_PUNCT.contains(&last_char) {
                        tok_end -= last_char.len_utf8();
                        end -= last_char.len_utf8();
                    } else {
                        break;
                    }
                }
                let path_str = tok[..tok_end].to_string();
                if !path_str.is_empty() {
                    out.push((start..end, path_str));
                }
                i = j;
                continue;
            }
        }
        // Advance by one char.
        match text[i..].chars().next() {
            Some(ch) => i += ch.len_utf8(),
            None => break,
        }
    }
    out
}

/// Resolve `raw` to an absolute path. Supports `~`, absolute paths, and
/// paths relative to `cwd`.
fn resolve_path(raw: &str, cwd: &Path) -> PathBuf {
    let expanded = shellexpand::tilde(raw).into_owned();
    let p = PathBuf::from(&expanded);
    if p.is_absolute() {
        p
    } else {
        cwd.join(&p)
    }
}

/// Truncate `s` to at most `max_bytes`, preserving UTF-8 boundaries.
fn truncate_at_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Expand `@path` mentions in `text` using `config`'s security policy and
/// cwd. See the module docs.
pub fn expand_mentions(text: &str, config: &Config) -> MentionExpansion {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    expand_mentions_with_cwd(text, config, &cwd)
}

/// Variant of [`expand_mentions`] that takes an explicit `cwd`, for tests.
pub fn expand_mentions_with_cwd(text: &str, config: &Config, cwd: &Path) -> MentionExpansion {
    let matches = find_mentions(text);
    if matches.is_empty() {
        return MentionExpansion {
            augmented_text: text.to_string(),
            cards: Vec::new(),
        };
    }

    let mut augmented = text.to_string();
    let mut cards: Vec<MentionCard> = Vec::new();
    let mut attachments = String::new();

    for (_range, raw_path) in &matches {
        let raw_token = format!("@{raw_path}");
        let resolved = resolve_path(raw_path, cwd);

        // For blocked-path check we need a canonical-ish path. is_blocked_path
        // handles non-existent paths by falling back to the literal form.
        let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

        if is_blocked_path(
            &canonical,
            &config.security.blocked_paths,
            &config.security.allowed_paths,
        ) {
            cards.push(MentionCard {
                raw_token,
                kind: MentionCardKind::Skipped {
                    display: raw_path.clone(),
                    reason: SkipReason::BlockedPath,
                },
            });
            continue;
        }

        if !canonical.exists() {
            cards.push(MentionCard {
                raw_token,
                kind: MentionCardKind::Skipped {
                    display: raw_path.clone(),
                    reason: SkipReason::NotFound,
                },
            });
            continue;
        }

        if canonical.is_dir() {
            let listing_args = serde_json::json!({
                "path": canonical.to_string_lossy(),
                "depth": 1,
            });
            match handle_list_dir(&listing_args, config) {
                Ok(listing) => {
                    attachments.push_str(&format!(
                        "\n\n<attached_dir path=\"{raw_path}\">\n{listing}</attached_dir>"
                    ));
                    cards.push(MentionCard {
                        raw_token,
                        kind: MentionCardKind::Dir {
                            display: raw_path.clone(),
                        },
                    });
                }
                Err(e) => cards.push(MentionCard {
                    raw_token,
                    kind: MentionCardKind::Skipped {
                        display: raw_path.clone(),
                        reason: SkipReason::ReadError(e.to_string()),
                    },
                }),
            }
            continue;
        }

        // File path.
        match std::fs::read_to_string(&canonical) {
            Ok(contents) => {
                let total_lines = if contents.is_empty() {
                    0
                } else {
                    contents.lines().count()
                };
                let (body, truncated) = if contents.len() > MAX_FILE_BYTES {
                    (truncate_at_bytes(&contents, MAX_FILE_BYTES), true)
                } else {
                    (contents.as_str(), false)
                };
                attachments.push_str(&format!("\n\n<attached_file path=\"{raw_path}\">\n{body}"));
                if truncated {
                    attachments.push_str(&format!(
                        "\n… [truncated at {MAX_FILE_BYTES} bytes — request a specific range]"
                    ));
                }
                attachments.push_str("\n</attached_file>");
                cards.push(MentionCard {
                    raw_token,
                    kind: MentionCardKind::File {
                        display: raw_path.clone(),
                        lines: total_lines,
                    },
                });
            }
            Err(e) => cards.push(MentionCard {
                raw_token,
                kind: MentionCardKind::Skipped {
                    display: raw_path.clone(),
                    reason: SkipReason::ReadError(e.to_string()),
                },
            }),
        }
    }

    augmented.push_str(&attachments);
    MentionExpansion {
        augmented_text: augmented,
        cards,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config::default();
        // Default blocked paths include `.ssh`, `.aws`, etc. — keep them.
        c.security.blocked_paths = vec![".ssh".to_string(), ".aws".to_string(), ".env".to_string()];
        c.security.allowed_paths = vec![];
        c
    }

    #[test]
    fn parses_basic_file_mention() {
        let m = find_mentions("see @foo.rs please");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, "foo.rs");
    }

    #[test]
    fn parses_relative_dir_mention_with_trailing_slash() {
        let m = find_mentions("look in @../govscout/");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, "../govscout/");
    }

    #[test]
    fn ignores_email_addresses() {
        // `@` not at start/after-whitespace → not a mention.
        let m = find_mentions("email me at foo@bar.com thanks");
        assert!(m.is_empty(), "emails must not match, got {m:?}");
    }

    #[test]
    fn strips_trailing_sentence_punctuation() {
        let m = find_mentions("what's in @foo.rs?");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].1, "foo.rs", "trailing `?` must be stripped from path");
    }

    #[test]
    fn strips_multiple_trailing_punct() {
        let m = find_mentions("see @foo.rs).");
        assert_eq!(m[0].1, "foo.rs");
    }

    #[test]
    fn keeps_dots_inside_filename() {
        let m = find_mentions("@foo.tar.gz done");
        assert_eq!(m[0].1, "foo.tar.gz");
    }

    #[test]
    fn handles_mention_at_start_of_line() {
        let m = find_mentions("@Cargo.toml what crates?");
        assert_eq!(m[0].1, "Cargo.toml");
    }

    #[test]
    fn multiple_mentions_in_one_message() {
        let m = find_mentions("diff @a.rs vs @b.rs");
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].1, "a.rs");
        assert_eq!(m[1].1, "b.rs");
    }

    #[test]
    fn expand_file_injects_attached_file_block() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("note.md");
        std::fs::write(&p, "line1\nline2\nline3\n").unwrap();
        let text = format!("summarize @{}", p.display());
        let out = expand_mentions_with_cwd(&text, &cfg(), tmp.path());
        assert!(
            out.augmented_text.contains("<attached_file"),
            "augmented text missing <attached_file> block: {}",
            out.augmented_text
        );
        assert!(out.augmented_text.contains("line1\nline2\nline3"));
        assert_eq!(out.cards.len(), 1);
        match &out.cards[0].kind {
            MentionCardKind::File { lines, .. } => assert_eq!(*lines, 3),
            other => panic!("expected File card, got {other:?}"),
        }
    }

    #[test]
    fn expand_dir_injects_attached_dir_listing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "b").unwrap();
        let text = format!("what's in @{}", tmp.path().display());
        let out = expand_mentions_with_cwd(&text, &cfg(), tmp.path());
        assert!(
            out.augmented_text.contains("<attached_dir"),
            "missing <attached_dir>: {}",
            out.augmented_text
        );
        assert!(out.augmented_text.contains("a.txt"));
        assert!(out.augmented_text.contains("b.txt"));
        assert_eq!(out.cards.len(), 1);
        assert!(matches!(out.cards[0].kind, MentionCardKind::Dir { .. }));
    }

    #[test]
    fn blocked_path_produces_skipped_card_and_leaves_text_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = tmp.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("id_rsa"), "SECRET").unwrap();
        let mut config = Config::default();
        config.security.blocked_paths = vec![".ssh".to_string()];
        let text = format!("@{}/id_rsa dump please", ssh.display());
        let out = expand_mentions_with_cwd(&text, &config, tmp.path());
        assert!(
            !out.augmented_text.contains("SECRET"),
            "blocked file contents must never be injected"
        );
        assert!(
            !out.augmented_text.contains("<attached_file"),
            "no attachment block should be added for blocked path"
        );
        assert_eq!(out.cards.len(), 1);
        match &out.cards[0].kind {
            MentionCardKind::Skipped {
                reason: SkipReason::BlockedPath,
                ..
            } => {}
            other => panic!("expected Skipped(BlockedPath), got {other:?}"),
        }
    }

    #[test]
    fn email_like_text_produces_no_expansion() {
        let out = expand_mentions_with_cwd("contact foo@bar.com", &cfg(), Path::new("."));
        assert_eq!(out.augmented_text, "contact foo@bar.com");
        assert!(out.cards.is_empty());
    }

    #[test]
    fn missing_path_produces_not_found_card_and_preserves_token() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "look at @does_not_exist_here.rs please";
        let out = expand_mentions_with_cwd(text, &cfg(), tmp.path());
        assert_eq!(out.augmented_text, text, "text must be unchanged");
        assert_eq!(out.cards.len(), 1);
        assert!(matches!(
            out.cards[0].kind,
            MentionCardKind::Skipped {
                reason: SkipReason::NotFound,
                ..
            }
        ));
    }

    #[test]
    fn large_file_is_truncated_but_line_count_reflects_full_file() {
        let tmp = tempfile::tempdir().unwrap();
        // Build a file > MAX_FILE_BYTES with a known line count.
        let line = "x".repeat(100); // 100 bytes + \n = 101
        let lines = (MAX_FILE_BYTES / 101) + 200; // ensures > MAX
        let content = (0..lines)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let p = tmp.path().join("big.txt");
        std::fs::write(&p, &content).unwrap();
        let text = format!("@{}", p.display());
        let out = expand_mentions_with_cwd(&text, &cfg(), tmp.path());
        assert!(out.augmented_text.contains("truncated at"));
        // Attached body must not exceed the cap by more than the marker
        // + tag overhead.
        let attached = &out.augmented_text;
        assert!(
            attached.len() < MAX_FILE_BYTES + 1024,
            "attached content not truncated: {} bytes",
            attached.len()
        );
        match &out.cards[0].kind {
            MentionCardKind::File { lines: n, .. } => {
                assert_eq!(
                    *n, lines,
                    "card should report FULL line count, not truncated"
                )
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn card_label_matches_screenshot_format() {
        let f = MentionCard {
            raw_token: "@CLAUDE.md".to_string(),
            kind: MentionCardKind::File {
                display: "CLAUDE.md".to_string(),
                lines: 275,
            },
        };
        assert_eq!(f.label(), "Read CLAUDE.md (275 lines)");

        let d = MentionCard {
            raw_token: "@foo/".to_string(),
            kind: MentionCardKind::Dir {
                display: "../../reference/codex/".to_string(),
            },
        };
        assert_eq!(d.label(), "Listed directory ../../reference/codex/");

        let s = MentionCard {
            raw_token: "@.ssh/".to_string(),
            kind: MentionCardKind::Skipped {
                display: ".ssh/".to_string(),
                reason: SkipReason::BlockedPath,
            },
        };
        assert_eq!(s.label(), "Skipped @.ssh/ (blocked path)");
    }

    #[test]
    fn single_line_uses_singular_noun() {
        let f = MentionCard {
            raw_token: "@x".to_string(),
            kind: MentionCardKind::File {
                display: "x".to_string(),
                lines: 1,
            },
        };
        assert_eq!(f.label(), "Read x (1 line)");
    }
}
