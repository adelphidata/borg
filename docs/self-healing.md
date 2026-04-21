# Self-Healing & Resilience

Borg is designed to run unattended for weeks at a time. The features below
exist so a self-hosted install stops accumulating the kind of silent
breakage that otherwise turns into "I'll just reinstall from scratch"
afternoons.

## The daily maintenance task

A scheduled task with `task_type = 'maintenance'` fires at **02:00 UTC**
every day (cron `0 0 2 * * *`, seeded in V37, id
`00000000-0000-4000-8000-ada1ca1e0001`). All seeded cron schedules are
evaluated in UTC regardless of the `timezone` column on the row — that
column is display-only today.
Unlike prompt/command tasks, it **does not invoke the LLM** — the task
dispatcher calls `borg_core::maintenance::run_daily_maintenance` directly.
That means it keeps running even when no provider key is configured and
costs zero tokens.

Each run does the following in order:

1. **Headless doctor sweep.** Runs the same 17 check categories as
   `borg doctor`, persists the full result to the `doctor_runs` table.
2. **Log rotation.** Deletes `~/.borg/logs/*.jsonl` files older than
   `maintenance.logs_retention_days` (default 30).
3. **Activity-log pruning.** Deletes `activity_log` rows older than
   `maintenance.activity_retention_days` (default 30).
4. **Embedding-cache pruning.** Evicts embeddings not accessed in the
   last 30 days.
5. **Stalled-task scan.** Recomputes `next_run` for any recurring
   scheduled task whose fire time has drifted more than one hour into
   the past (see below).
6. **Persistent-warning surfacing.** Compares this run's Warn/Fail
   checks against the previous run; any issue that appeared in both is
   added to `MaintenanceReport.persistent_warnings`. Single-run flukes
   do not escalate — only nags that stick.

The run is recorded to `doctor_runs`, and the table is capped at
`maintenance.doctor_runs_keep` (default 30) rows.

### Disabling

```sh
borg set maintenance.enabled false
```

With `enabled = false` the task still fires but `run_daily_maintenance`
returns immediately after logging a skip message.

### Config keys

| Key | Default | Meaning |
|-----|---------|---------|
| `maintenance.enabled` | `true` | Master switch for the daily sweep. |
| `maintenance.logs_retention_days` | `30` | `~/.borg/logs/*.jsonl` files older than this are deleted. |
| `maintenance.activity_retention_days` | `30` | `activity_log` rows older than this are deleted. |
| `maintenance.doctor_runs_keep` | `30` | Keep at most this many `doctor_runs` history rows. |

All four are wired through `/settings` under the **Maintenance**
section.

## Missed-run detection for scheduled tasks

The daemon loop scans `scheduled_tasks` every 5 minutes for rows that:

- have `status = 'active'`,
- have `retry_after IS NULL` (not currently in retry backoff),
- have `schedule_type != 'once'` (one-shots are allowed to sit),
- have `next_run` more than `STALLED_TASK_GRACE_SECS` (1 hour) in the
  past without firing.

For each stalled task, the daemon:

1. Inserts a `task_runs` row with `status = 'missed'` for audit trail.
2. Recomputes `next_run` from the cron / interval expression at the
   current time so the task will fire on the next normal cadence.

**The missed run is not replayed.** The assumption is that the user
wants "fire on schedule from here on," not "catch up on everything we
lost during the outage." If you need catch-up semantics, use a prompt
task that queries the current state and reconciles explicitly.

### Troubleshooting

If you suspect a task is silently stalling, check its recent history:

```sql
SELECT task_id, started_at, status, error
FROM task_runs
WHERE status = 'missed'
ORDER BY started_at DESC
LIMIT 20;
```

`task_runs.error` on a missed row contains the drift in seconds, e.g.
`self-healing: next_run drifted 86400s into past without firing`.

The maintenance sweep runs the same scan, so a single manual invocation
of `borg schedule run <maintenance-task-id>` will heal stalled tasks
even when the daemon isn't running.

## Memory entry size limits

`write_memory` rejects any write that would produce an entry larger than
**20,000 tokens** and warns in logs at **8,000 tokens**. Both thresholds
live in `crates/core/src/constants.rs` as
`MEMORY_ENTRY_REJECT_TOKENS` / `MEMORY_ENTRY_WARN_TOKENS`.

Why a hard cap? The token-budget loader that populates the system prompt
has a fixed ceiling. Entries beyond the budget get silently dropped at
load time, so a too-large `MEMORY.md` or topic file just... isn't in
context, with no error surfaced anywhere. Failing loud at write time
forces the caller (usually the LLM, via the `write_memory` tool) to
split the content into topic-sized pieces — which is the shape the
consolidation crons already expect.

Append mode checks the **combined** size (existing + new), so appending
4k tokens to a 17k-token entry fails just like writing 21k fresh.

## Related systems

- **Consolidation crons** (`docs/memory.md` — nightly 03:00 / weekly
  Sunday 04:00) deduplicate and merge memory entries. The daily
  maintenance task runs at 02:00 so logs/activity pruning happens
  before consolidation reads from those tables.
- **Heartbeat** (`docs/heartbeat.md`) uses the same scheduler. If
  heartbeat stops firing, the stalled-task scan will not detect it
  because heartbeat is not a row in `scheduled_tasks`; check
  `daemon_lock.heartbeat_at` via `borg doctor` instead.
- **Doctor** (CLI: `borg doctor`) runs the same `DiagnosticRunner` the
  maintenance task uses — the output format just differs.
