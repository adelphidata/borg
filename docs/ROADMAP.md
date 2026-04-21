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

All three post-Tier-1 gaps are now shipped. See `docs/self-healing.md`
for current behavior:

- ~~**Stuck `running` task_runs while daemon stays up.**~~ The 5-min
  daemon tick calls `recover_wedged_runs`, which fails any `running`
  row whose `started_at` is older than the task's `timeout_ms`
  (falling back to `STALLED_TASK_GRACE_SECS`).
- ~~**Clock-jump storm.**~~ `heal_stalled_tasks` collapses audit rows
  into one aggregate entry when more than
  `CLOCK_JUMP_AGGREGATE_THRESHOLD` (5) tasks drift in a single sweep.
- ~~**Timezone drift on seeded crons.**~~ `calculate_next_run` honors
  IANA names on the `timezone` column via `chrono-tz`. `"local"`,
  `""`, and `"UTC"` still map to UTC so existing seeded rows keep
  their original semantics; unparseable zones fall back to UTC with
  a `tracing::warn!`.

## /btw — non-blocking side questions

Shipped (TUI-only): `/btw <question>` spawns a tool-less, non-persistent side agent that answers using the current session's transcript snapshot. Result renders in a dismissable modal popup. See `crates/core/src/agent/btw.rs` and `crates/cli/src/tui/btw_popup.rs`.

Deferred:

- **Gateway / channel surface.** Mirror `run_btw` into `crates/gateway/src/handler.rs` so Telegram, Slack, Discord, iMessage, etc. can handle a `/btw ` prefix before routing to the main agent. Reply should land as a threaded follow-up so it doesn't derail the main conversation.
