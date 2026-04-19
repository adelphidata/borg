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

use super::{
    compute_mood, compute_readiness, format_compact, format_evolution_overview,
    format_next_step_hints, format_xp_feed, format_xp_summary, next_step_hints, recent_xp_feed,
    xp_summary,
};
use crate::bond;
use crate::db::Database;
use crate::vitals;

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
            // Load vitals + bond so mood, readiness, and hints can be
            // derived. All three surfaces are lossy UX signals — fall back
            // to baseline state on DB failure and log the reason instead of
            // silently swallowing.
            let now = chrono::Utc::now();
            let vitals_state = match db.get_vitals_state() {
                Ok(v) => vitals::apply_decay(&v, now),
                Err(e) => {
                    tracing::warn!("evolution: vitals unavailable, using baseline: {e}");
                    vitals::baseline()
                }
            };
            let bond_state = match db.get_all_bond_events() {
                Ok(events) => {
                    let key = db.derive_hmac_key(bond::BOND_HMAC_DOMAIN);
                    bond::replay_events_with_key(&key, &events)
                }
                Err(e) => {
                    tracing::warn!("evolution: bond events unavailable, using empty: {e}");
                    bond::replay_events_with_key(&[], &[])
                }
            };

            let mut state = state;
            state.mood = Some(compute_mood(&state, &vitals_state, &bond_state));
            state.readiness = compute_readiness(&state, &vitals_state, &bond_state);
            let hints = next_step_hints(&state, &vitals_state, &bond_state);

            let mut text = format_evolution_overview(&state);
            if !hints.is_empty() {
                text.push('\n');
                text.push_str(&format_next_step_hints(&hints));
            }
            Ok(CommandOutput {
                text,
                image_png: None,
            })
        }
        EvolutionCommand::Xp => {
            let events = db.load_all_evolution_events()?;
            let now = chrono::Utc::now().timestamp();
            let summary = xp_summary(&events, now);
            let feed = recent_xp_feed(&events, 10);
            let mut text = String::new();
            text.push_str(&format_xp_summary(&summary));
            text.push('\n');
            text.push_str(&format_xp_feed(&feed));
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

    #[test]
    fn dispatch_xp_renders_all_sections() {
        let db = Database::test_db();
        db.record_evolution_event("xp_gain", 2, Some("builder"), "apply_patch", None)
            .expect("record xp_gain");
        db.record_evolution_event("xp_gain", 1, Some("ops"), "run_shell", None)
            .expect("record xp_gain");

        let out = dispatch(EvolutionCommand::Xp, &db).expect("dispatch xp");
        assert!(out.image_png.is_none());
        assert!(out.text.contains("XP Summary"), "text was: {}", out.text);
        assert!(out.text.contains("Top sources"));
        assert!(out.text.contains("Archetypes"));
        assert!(out.text.contains("Recent XP"));
        assert!(out.text.contains("apply_patch"));
        assert!(out.text.contains("run_shell"));
    }

    #[test]
    fn dispatch_xp_empty_db_still_renders() {
        let db = Database::test_db();
        let out = dispatch(EvolutionCommand::Xp, &db).expect("dispatch xp");
        assert!(out.text.contains("XP Summary"));
        assert!(out.text.contains("Recent XP"));
        assert!(out.text.contains("no recent activity"));
    }
}
