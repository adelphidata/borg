//! Shared `/evolution` · `/xp` · `/card` command dispatcher.
//!
//! Single source of truth for the V2 status surfaces so the TUI and channel
//! gateways render identical output. The TUI wraps [`CommandOutput::text`]
//! in a popup; the gateway interceptor relays it as a plain-text reply and
//! attaches [`CommandOutput::image_png`] if the channel supports media.
//!
//! Phase 1 lays down the dispatcher skeleton with minimal stub output so
//! Phase 2 streams (A/B/E) can fill in the formatting without changing
//! signatures. The stubs render the raw state so Phase 1 is still callable
//! end-to-end.

use std::path::PathBuf;

use anyhow::Result;

use super::{format_archetype_scores, format_compact, format_status_section, xp_summary};
use crate::db::Database;

/// A slash-command recognised by the evolution surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvolutionCommand {
    /// `/evolution` — overview of stage, level, archetype, readiness.
    Evolution,
    /// `/xp` — today/week totals, top sources, archetype breakdown, feed.
    Xp,
    /// `/card` — shareable ASCII card; optional `--out <path>` writes to a file.
    Card {
        /// Output path for `--out`. `None` prints to stdout (CLI) or replies
        /// inline (channels).
        out: Option<PathBuf>,
    },
}

/// Rendered output of a dispatched command.
///
/// `image_png` stays in the struct so Phase-2 / later-iteration PNG share
/// cards can land without a signature change. Unused today.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// Plain-text rendering suitable for TUI popups and channel replies.
    pub text: String,
    /// Optional PNG payload for channels that support media attachments.
    pub image_png: Option<Vec<u8>>,
}

/// Parse an inbound message into an [`EvolutionCommand`], if any.
///
/// Recognises the leading token only — anything after is treated as args.
/// Leading/trailing whitespace is ignored; matches are case-insensitive on
/// the command name. Returns `None` when the message isn't a known command.
pub fn parse(msg: &str) -> Option<EvolutionCommand> {
    let trimmed = msg.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.split_whitespace();
    let head = parts.next()?.to_lowercase();
    match head.as_str() {
        "/evolution" => Some(EvolutionCommand::Evolution),
        "/xp" => Some(EvolutionCommand::Xp),
        "/card" => {
            let mut out: Option<PathBuf> = None;
            while let Some(tok) = parts.next() {
                if tok == "--out" {
                    if let Some(path) = parts.next() {
                        out = Some(PathBuf::from(path));
                    }
                }
            }
            Some(EvolutionCommand::Card { out })
        }
        _ => None,
    }
}

/// Dispatch a parsed command against the given database, returning rendered
/// output.
///
/// Phase 1 emits minimal but coherent text so the surfaces are exercised
/// end-to-end. Streams A/B/E replace each branch's body with the real
/// renderer from `evolution/format.rs` / `evolution/share_card.rs`.
pub fn dispatch(cmd: EvolutionCommand, db: &Database) -> Result<CommandOutput> {
    let state = db.get_evolution_state()?;

    match cmd {
        EvolutionCommand::Evolution => {
            let mut text = String::new();
            text.push_str(&format_status_section(&state));
            text.push('\n');
            text.push_str(&format_archetype_scores(&state));
            Ok(CommandOutput {
                text,
                image_png: None,
            })
        }
        EvolutionCommand::Xp => {
            let events = db.load_all_evolution_events()?;
            let now = chrono::Utc::now().timestamp();
            let summary = xp_summary(&events, now);
            let mut text = String::new();
            text.push_str(&format!(
                "XP · today {} · week {}\n",
                summary.today, summary.week
            ));
            if !summary.top_sources.is_empty() {
                text.push_str("\nTop sources (7d):\n");
                for (src, xp) in &summary.top_sources {
                    text.push_str(&format!("  {src:<20} {xp}\n"));
                }
            }
            Ok(CommandOutput {
                text,
                image_png: None,
            })
        }
        EvolutionCommand::Card { out: _ } => {
            // Phase-1 stub: reuse the compact one-liner until Stream E lands
            // the real ASCII share-card renderer.
            let text = format!("{}\n", format_compact(&state));
            Ok(CommandOutput {
                text,
                image_png: None,
            })
        }
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_evolution() {
        assert_eq!(parse("/evolution"), Some(EvolutionCommand::Evolution));
        assert_eq!(parse("  /evolution  "), Some(EvolutionCommand::Evolution));
        assert_eq!(parse("/Evolution"), Some(EvolutionCommand::Evolution));
    }

    #[test]
    fn parse_xp() {
        assert_eq!(parse("/xp"), Some(EvolutionCommand::Xp));
        assert_eq!(parse("/XP extra args"), Some(EvolutionCommand::Xp));
    }

    #[test]
    fn parse_card_without_out() {
        assert_eq!(parse("/card"), Some(EvolutionCommand::Card { out: None }));
    }

    #[test]
    fn parse_card_with_out() {
        let got = parse("/card --out /tmp/x.txt");
        assert_eq!(
            got,
            Some(EvolutionCommand::Card {
                out: Some(PathBuf::from("/tmp/x.txt"))
            })
        );
    }

    #[test]
    fn parse_unknown_returns_none() {
        assert!(parse("/mode").is_none());
        assert!(parse("hi there").is_none());
        assert!(parse("").is_none());
    }
}
