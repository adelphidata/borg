# Roadmap

Tracked follow-ups from shipped work. Each entry names the source plan or
commit that deferred the work so context isn't lost.

## Self-healing — Tier 2 / Tier 3

Deferred from the "Tier 1 — core self-healing" plan (see `docs/self-healing.md`
and commit `e666f4c`). Tier 1 shipped missed-run detection, the memory size
cap, and the daily maintenance sweep. The following were explicitly left
out to avoid refactor sprawl:

- **Skill audit log / unknown-skill detection.** Warn when a skill appears
  under `~/.borg/skills/` that wasn't installed via the plugin system —
  defends against supply-chain tampering.
- **Gateway async-blocking fixes.** Replace synchronous `std::fs::read_to_string`
  calls inside async hot paths at `crates/gateway/src/manifest.rs:169` and
  `crates/gateway/src/imessage/monitor.rs:282`. These can stall the event
  loop under slow disk I/O.
- **Persistent doctor-warning TUI banner.** Surface
  `MaintenanceReport.persistent_warnings` as a non-dismissable banner in
  the TUI (and/or deliver to the configured heartbeat channel). Today the
  report is written to `doctor_runs` but the user still has to query it
  by hand.
- **CVE advisory integration.** Pull from rustsec / OSV on a cadence and
  flag vulnerable dependencies from a doctor check.

## Self-healing — known gaps worth tracking

Surfaced by the post-ship code review of Tier 1:

- **Stuck `running` task_runs while daemon stays up.** `recover_stale_runs`
  only fires on daemon startup. If the daemon stays alive but a single
  task execution wedges (panic inside `spawn_blocking`, network hang past
  the timeout), the row sits in `running` forever. Consider a
  heartbeat/timeout sweep inside the daemon loop, not just at startup.
- **Clock-jump storm.** After a long laptop sleep, every recurring task
  looks stalled at once and we record one `missed` row per task. Correct
  but noisy — consider a per-sweep cap or a single aggregate row when
  more than N tasks drift in the same scan.
- **Timezone drift on seeded crons.** All seeded cron schedules evaluate
  in UTC today (the `timezone` column is display-only). If we ever want
  "02:00 in the user's local time" semantics, `calculate_next_run` needs
  to honor the column — not just expose it.

## /btw — non-blocking side questions

Shipped (TUI-only): `/btw <question>` spawns a tool-less, non-persistent side agent that answers using the current session's transcript snapshot. Result renders in a dismissable modal popup. See `crates/core/src/agent/btw.rs` and `crates/cli/src/tui/btw_popup.rs`.

Deferred:

- **Gateway / channel surface.** Mirror `run_btw` into `crates/gateway/src/handler.rs` so Telegram, Slack, Discord, iMessage, etc. can handle a `/btw ` prefix before routing to the main agent. Reply should land as a threaded follow-up so it doesn't derail the main conversation.
- **Persisted `/btw` log.** Optionally record the side Q/A to a new `btw_entries` table, viewable via a `/btw log` subcommand. Today dismissing the popup loses the answer (matches hermes-agent behavior).
- **Tool-enabled variant.** Allow a read-only subset (`read_file`, `memory_search`, `web_fetch`) behind a `btw.tools_enabled` config flag. Kept off by default — unpredictable latency and surprise side effects are the exact things `/btw` is supposed to avoid.
- **Rate limiting.** Cap N `/btw`s per minute per session to stop accidental runaway loops burning tokens.
- **Multi-answer stack.** Queue popups instead of the current single-slot cancel-in-flight behavior so rapidly fired `/btw`s don't clobber each other.
