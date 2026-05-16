//! Unified command router for the `interface` crate.
//!
//! Today the same operation (e.g. open settings, run heal, toggle plan mode)
//! is wired three times: as a clap subcommand in `main.rs`, as a slash
//! command in `tui/commands.rs`, and as a `/cmd` entry in the TUI command
//! popup. The registry below is the single source of truth for command
//! identity (name, aliases, surface flags, help text) plus the canonical
//! action implementation.
//!
//! The registry is intentionally additive for now: existing CLI/REPL/TUI
//! match arms still compile and dispatch unchanged. The proof of the seam
//! is `/heal`, which has been migrated end-to-end — the CLI subcommand and
//! the TUI slash command both call [`run_heal_action`] via the registry
//! lookup, so the format string lives in exactly one place.
//!
//! Migrating the remaining commands is a follow-up; do not try to land
//! them in the same change.
//!
//! Surface key: `cli` = clap subcommand in `main.rs`, `tui_slash` = `/cmd`
//! handled in `tui/commands.rs`, `tui_key` = bound key in `tui/app/key_handlers.rs`.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::Mutex;

use crate::daemon_client::DaemonClient;

/// Which UI surfaces a command is exposed through. Used by each frontend
/// to decide whether to show the command in its discovery UI (e.g. the TUI
/// command popup) and to assert at lookup time that the surface is
/// supported.
///
/// `dead_code` is allowed for now: only one command (`heal`) is migrated
/// in this seam, and only the lookup tests read the flags. The follow-up
/// migration that moves the full TUI command popup onto the registry will
/// wire these into production code paths.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct Surfaces {
    pub cli: bool,
    pub tui_slash: bool,
    pub tui_key: bool,
}

/// Outcome of a registry-driven action. Each surface adapts this to its
/// own rendering: CLI prints to stdout, TUI pushes a system-message cell.
#[derive(Debug, Clone)]
pub enum CommandOutcome {
    /// A multi-line string the caller should display verbatim. Production
    /// commands that fetch a daemon report and render it land here.
    Text(String),
}

/// Static description of a command. Handlers are *not* function pointers
/// because most actions need access to surface-specific state (the TUI
/// `App`, a `DaemonClient`, etc.) — instead, each surface looks up the
/// def, then calls the matching `run_*_action` free function in this
/// module. That keeps the registry data-only (no closures over `Send +
/// Sync` lifetimes) and makes the dispatch table grep-friendly.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // see `Surfaces` for migration-stage rationale
pub struct CommandDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    pub surfaces: Surfaces,
}

/// The registry. Append new commands here when you migrate them.
pub static REGISTRY: &[CommandDef] = &[CommandDef {
    name: "heal",
    aliases: &[],
    help: "Run the self-healing maintenance sweep on demand.",
    surfaces: Surfaces {
        cli: true,
        tui_slash: true,
        tui_key: false,
    },
}];

/// Look up a command by its canonical name or any alias. The leading slash
/// (TUI form) is stripped if present so callers can pass either `"heal"`
/// or `"/heal"`.
pub fn lookup(name_or_alias: &str) -> Option<&'static CommandDef> {
    let needle = name_or_alias.strip_prefix('/').unwrap_or(name_or_alias);
    REGISTRY
        .iter()
        .find(|cmd| cmd.name == needle || cmd.aliases.contains(&needle))
}

/// Canonical implementation of the `heal` action. Both the CLI subcommand
/// (`borg heal`) and the TUI slash command (`/heal`) call this; the format
/// string lives in [`format_heal_report`] so the two surfaces cannot drift.
pub async fn run_heal_action(client: Arc<Mutex<DaemonClient>>) -> Result<CommandOutcome> {
    let report = {
        let mut c = client.lock().await;
        c.heal().await?
    };
    Ok(CommandOutcome::Text(format_heal_report(&report)))
}

/// Render a daemon-side [`protocols::admin::HealReport`] as a multi-line
/// human-readable summary. Single source of truth — the CLI and TUI
/// renderers were copies of each other before this consolidation, and
/// drifted in their phrasing of the activity line. This is the canonical
/// version both surfaces use.
pub fn format_heal_report(report: &protocols::admin::HealReport) -> String {
    let mut text = if report.skipped {
        String::from("Self-healing sweep skipped\n───────────────────────────\n")
    } else {
        String::from("Self-healing sweep complete\n───────────────────────────\n")
    };
    if report.skipped {
        text.push_str("maintenance.enabled is false");
    } else {
        text.push_str(&format!(
            "Pruned {} log files ({} bytes truncated), {} activity rows, evicted {} embeddings, healed {} stalled tasks, recovered {} wedged runs, purged {} messages, tombstoned {} audit events",
            report.pruned_logs,
            report.log_bytes_truncated,
            report.pruned_activity,
            report.evicted_embeddings,
            report.healed_tasks,
            report.wedged_runs_recovered,
            report.messages_purged,
            report.audit_events_tombstoned,
        ));
    }
    if !report.warnings.is_empty() {
        text.push_str("\n\nWarnings:");
        for w in &report.warnings {
            text.push_str("\n  • ");
            text.push_str(w);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_finds_canonical_name() {
        let def = lookup("heal").expect("heal must be registered");
        assert_eq!(def.name, "heal");
        assert!(def.surfaces.cli);
        assert!(def.surfaces.tui_slash);
    }

    #[test]
    fn lookup_strips_leading_slash() {
        let with_slash = lookup("/heal").expect("/heal must resolve");
        let without_slash = lookup("heal").expect("heal must resolve");
        assert_eq!(with_slash.name, without_slash.name);
    }

    #[test]
    fn lookup_resolves_aliases() {
        // Synthetic alias coverage: walk the registry, and for each command
        // with declared aliases, every alias must round-trip back to the
        // same canonical entry. Guards against accidentally adding aliases
        // to the static table without wiring them through `lookup`.
        for cmd in REGISTRY {
            for alias in cmd.aliases {
                let resolved = lookup(alias).unwrap_or_else(|| {
                    panic!(
                        "alias {alias:?} for {:?} must round-trip via lookup()",
                        cmd.name
                    )
                });
                assert_eq!(
                    resolved.name, cmd.name,
                    "alias {alias:?} resolved to {:?}, expected {:?}",
                    resolved.name, cmd.name
                );
            }
        }
    }

    #[test]
    fn lookup_returns_none_for_unknown() {
        assert!(lookup("definitely-not-a-command").is_none());
        assert!(lookup("/definitely-not-a-command").is_none());
    }

    #[test]
    fn registry_entries_carry_help_and_surfaces() {
        // The registry's whole point is to centralise per-command metadata
        // for the three frontends. Drop these fields and the unified
        // command popup / help generator silently loses its source data —
        // assert we always carry them so a future migration can read them.
        for cmd in REGISTRY {
            assert!(
                !cmd.help.is_empty(),
                "command {:?} must declare help text",
                cmd.name
            );
            let s = cmd.surfaces;
            assert!(
                s.cli || s.tui_slash || s.tui_key,
                "command {:?} is registered but exposed on no surface",
                cmd.name
            );
        }
    }

    #[test]
    fn registry_names_are_unique() {
        // A duplicated name would silently let one entry shadow the other
        // depending on iteration order — better to fail loudly at test
        // time than to debug "why is /heal calling the wrong handler".
        let mut names: Vec<&str> = REGISTRY.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "REGISTRY contains duplicate names");
    }

    #[test]
    fn format_heal_report_renders_skipped() {
        let out = format_heal_report(&protocols::admin::HealReport {
            skipped: true,
            ..Default::default()
        });
        assert!(out.starts_with("Self-healing sweep skipped"), "{out:?}");
        assert!(out.contains("maintenance.enabled is false"), "{out:?}");
    }

    #[test]
    fn format_heal_report_renders_counts_and_warnings() {
        let out = format_heal_report(&protocols::admin::HealReport {
            pruned_logs: 2,
            pruned_activity: 3,
            evicted_embeddings: 4,
            healed_tasks: 5,
            messages_purged: 6,
            wedged_runs_recovered: 7,
            log_bytes_truncated: 1024,
            audit_events_tombstoned: 11,
            warnings: vec!["something fishy".into()],
            ..Default::default()
        });
        assert!(out.starts_with("Self-healing sweep complete"));
        assert!(out.contains("Pruned 2 log files (1024 bytes truncated)"));
        assert!(out.contains("3 activity rows"));
        assert!(out.contains("evicted 4 embeddings"));
        assert!(out.contains("healed 5 stalled tasks"));
        assert!(out.contains("recovered 7 wedged runs"));
        assert!(out.contains("purged 6 messages"));
        assert!(out.contains("tombstoned 11 audit events"));
        assert!(out.contains("• something fishy"));
    }
}
