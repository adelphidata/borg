# Self-Healing & Resilience

Borg is designed to run unattended for weeks at a time. The features below
exist so a self-hosted install stops accumulating the kind of silent
breakage that otherwise turns into "I'll just reinstall from scratch"
afternoons.

## The daily maintenance task

A scheduled task with `task_type = 'maintenance'` fires at **02:00 UTC**
every day (cron `0 0 2 * * *`, id
`00000000-0000-4000-8000-ada1ca1e0001`). Seeded rows store
`timezone = "local"`, which `calculate_next_run` treats as UTC (along
with `""` and `"UTC"`) to preserve the original fire-time semantics.
Any other value is parsed as an IANA zone name (e.g.
`America/New_York`) via `chrono-tz`; an unparseable name falls back to
UTC with a `tracing::warn!` — a corrupted timezone field must never
stop a task from firing.
Unlike prompt/command tasks, it **does not invoke the LLM** — the task
dispatcher calls `cortex::maintenance::run_daily_maintenance` directly.
That means it keeps running even when no provider key is configured and
costs zero tokens.

Each run does the following in order:

1. **Bootstrap repair.** Recreates the expected runtime subdirectories
   under `~/.borg/` (`channels`, `skills`, `hooks`, `backups`, `logs`)
   and seeds `global/INDEX` when long-term memory entries exist without
   an index. These are idempotent, cheap repairs that make fresh installs
   and "user deleted a subdir by hand" incidents self-correcting.
2. **Headless doctor sweep.** Runs the same 17 check categories as
   `borg doctor` (including a `PRAGMA integrity_check` under the
   **Data** category) and persists the full result to the `doctor_runs`
   table.
3. **Log rotation.** Deletes `~/.borg/logs/*.jsonl` files older than
   `maintenance.logs_retention_days` (default 30). In the same step,
   head-truncates the append-only logs `daemon.log`, `daemon.err`, and
   `tui.log` when they exceed 5 MB, keeping the last ~1 MB starting at a
   newline. These files are never rotated by the runtime — a single
   noisy warn loop once ballooned `daemon.log` past 20 MB in under two
   weeks; the cap prevents recurrence. Bytes freed are reported via
   `MaintenanceReport.log_bytes_truncated`.
4. **Activity-log pruning.** Deletes `activity_log` rows older than
   `maintenance.activity_retention_days` (default 30).
5. **Embedding-cache pruning.** Evicts embeddings not accessed in the
   last 30 days.
6. **Retention purge.** When `data.purge_policy = Auto` and
   `data.retention_days` is set, deletes old `messages` rows and
   tombstones old `audit_log_chain` rows in place so the hash chain
   remains verifiable while older payloads age out.
7. **Weekly auto-backup.** When `data.backup_keep_count > 0` and no
   backup has been created in the last 7 days, writes a
   `borg-backup-<timestamp>.tar.zst` snapshot under `~/.borg/backups/`
   and prunes older backups past the keep count. When
   `backup_keep_count = 0`, maintenance prunes any leftover auto-backup
   files instead of creating new ones.
8. **Stalled-task scan.** Recomputes `next_run` for any recurring
   scheduled task whose fire time has drifted more than one hour into
   the past (see below). When more than
   `CLOCK_JUMP_AGGREGATE_THRESHOLD` (5) tasks look stalled in a single
   sweep — the usual signature of a laptop waking from long sleep —
   the audit trail collapses into **one** aggregate `task_runs` row
   (`status='missed'`) instead of N near-identical ones. `next_run`
   is still recomputed for every task; only the audit entry is
   aggregated. `HealReport.aggregated` flags the case for the daemon
   log.
9. **Wedged-run recovery.** Marks any `task_runs` row stuck in
   `status='running'` past its timeout as `failed`, using the task's own
   `timeout_ms` when present and the 1-hour default grace otherwise.
10. **Remediation registry.** Any doctor issue marked `AutoSafe` is
    eligible for one bounded repair attempt during this sweep. Current
    safe remediations are recreating runtime subdirs, seeding
    `global/INDEX`, and repairing stale/missing memory embeddings and
    chunk indexes.
11. **Issue lifecycle update.** Warn/Fail checks are written into
    `maintenance_issue_state`, which tracks `first_seen_at`,
    `last_seen_at`, `consecutive_count`, and the last auto-fix attempt.
    `MaintenanceReport.persistent_warnings` is derived from that table:
    only still-open issues with `consecutive_count >= 2` are surfaced as
    persistent warnings. Single-run flukes do not escalate. Findings the
    user has squelched via `/doctor squelch` (see below) are filtered
    out here so they do not surface.
12. **Doctor-squelch expiry.** Removes rows from `doctor_squelches`
    whose `expires_at` has passed. The corresponding finding starts
    firing again on the next heartbeat / sweep — security squelches are
    intentionally bounded so an acknowledged risk doesn't fall off the
    radar permanently. TTL is
    `maintenance.security_squelch_ttl_days` (default 30).

## Doctor squelches

Some `/doctor` findings are intentional on a given host — e.g. the user
runs without macOS Application Firewall because they're behind a
hardware firewall. To stop those from re-surfacing in every heartbeat
without losing the signal entirely, the user can squelch a finding by
its `Category:Name` key:

```
/doctor                                 # find the key (printed under each Warn/Fail)
/doctor squelch Host Security:Application Firewall
/doctor squelches                       # show what's currently muted
/doctor unsquelch Host Security:Application Firewall
```

A squelch persists in the `doctor_squelches` SQLite table and is
filtered from:

- the heartbeat security nudge (`heartbeat_augmenters::check_critical_host_security`),
- the persistent-warning surface in the daily maintenance sweep.

It is **not** filtered from the explicit `/doctor` output — that
command always shows everything, with the squelch state visible in
`/doctor squelches`. Squelches auto-expire after
`maintenance.security_squelch_ttl_days` so a once-acknowledged risk
periodically returns for review.

The run is still recorded to `doctor_runs`, and the table is capped at
`maintenance.doctor_runs_keep` (default 30) rows. `doctor_runs` remains
the immutable per-sweep audit snapshot; `maintenance_issue_state` is the
mutable "latest lifecycle state" index across sweeps.

## Remediation model

Each doctor check may declare one of four remediation modes:

- `None` — pure diagnostics, no remediation metadata.
- `AutoSafe` — safe to repair during `/heal` or the nightly sweep.
- `AutoGuarded` — candidates for automation later, intentionally manual today.
- `Manual` — requires explicit operator action.

`borg doctor` shows fixability inline (`[auto-fixable]`, `[manual]`, etc.)
so the user can see whether a warning is expected to clear on the next
maintenance sweep or needs real intervention.

## Embedding repair

The maintenance sweep now repairs memory embedding backlog in addition to
pruning the embedding cache.

- Coverage is checked across all memory scopes known to the config or DB,
  not just `global`.
- The sweep repairs both `memory_embeddings` rows (whole-entry coverage)
  and `memory_chunks` rows / chunk embeddings (the retrieval path used by
  hybrid search).
- Repairs are bounded by two settings so `/heal` never turns into an
  unbounded background reindex:
  `maintenance.embedding_repair_max_files` and
  `maintenance.embedding_repair_max_chunks`.
- If no embedding-capable provider is available, maintenance records the
  issue but skips repair rather than failing the whole sweep.

### On-demand sweep

Run the full sweep immediately without waiting for the 02:00 scheduled
fire:

```text
/heal
```

The slash command goes through the daemon's `Admin.Heal` RPC, which
invokes `run_daily_maintenance` server-side — same code path as the
scheduled task, same `doctor_runs` row written, same underlying
maintenance behavior. The TUI renders the daemon's `HealReport`
projection plus any persistent warnings inline.

Unlike `/doctor` (read-only diagnostics, streamed), `/heal` executes
the mutating steps: bootstrap repair, log truncation, activity-log
pruning, embedding eviction, embedding backfill, workflow pruning,
retention purge, backup creation/pruning, stalled-task healing, and
wedged task-run recovery. Use `/doctor` when you want a quick health
readout; use `/heal` when you want the sweep to actually do its
housekeeping.

### Disabling

```sh
borg settings set maintenance.enabled false
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
| `maintenance.embedding_repair_enabled` | `true` | Whether maintenance repairs stale/missing memory embeddings and chunk indexes. |
| `maintenance.embedding_repair_max_files` | `16` | Maximum memory entries repaired in one sweep. |
| `maintenance.embedding_repair_max_chunks` | `256` | Maximum chunks rebuilt in one sweep. |
| `maintenance.issue_state_keep_days` | `30` | Resolved `maintenance_issue_state` rows older than this are pruned. |

All nine are wired through `/settings` under the **Maintenance**
section.

## Wedged-run recovery inside the daemon loop

On daemon startup, `recover_stale_runs` flips every `running` task_run
from a crashed daemon to `failed`. That recovery is startup-only — if
the daemon stays alive but a single task execution wedges (panic
inside `spawn_blocking`, network hang past its timeout), the row sat
in `running` forever until the next restart.

The same 5-minute tick that runs the stalled-task scan also calls
`recover_wedged_runs(db, now, STALLED_TASK_GRACE_SECS)`, and the daily
maintenance sweep runs that same wedged-run recovery path. That marks
any `running` task_run as `failed` when `started_at` is older than
the task's own `timeout_ms` (in seconds), falling back to
`default_grace_secs` (1h) when no per-task timeout is available. The
statement is a single SQL `UPDATE ... WHERE id IN (SELECT ... LEFT
JOIN scheduled_tasks ...)` so cost is bounded regardless of the
number of active tasks. Recovered rows carry
`error = 'self-healing: task_run wedged in ''running'' past timeout …'`.

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
live in `crates/cortex/src/constants.rs` as
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

## Skill tamper audit

User skills under `~/.borg/skills/<name>/SKILL.md` are hand-curated —
borg has no plugin-system install for skills, so the directory contents
are whatever the user put there. To catch post-install tampering, every
`borg doctor` run (and the daily maintenance sweep) hashes each
`SKILL.md` with SHA-256 and compares against the previous hash recorded
in the `skill_audit` table.

Model:

- **First observation** of a skill name inserts a row with no warning.
  The user put the skill there; trusting on first use is the only
  sensible default when there is no signed manifest to compare against.
- **Unchanged** (same hash) updates `last_seen_at` only.
- **Modified** (hash diverged) updates the stored hash and emits a
  `tracing::warn!` at observation time. The doctor Skills check then
  surfaces a `Warn` naming the affected skill(s) with the hint
  `review content for supply-chain tampering`.

Uninstalling a skill is a manual operation today (`rm -rf`); call
`db.forget_skill_audit(name)` if you need a reinstall to be treated as
first-seen rather than modified.

The helper that drives this is
`cortex::skill_security::audit_user_skills(&db, &dir)` — it records
observations into `skill_audit` and returns `Vec<SkillAuditFinding>`.

## Persistent-warning notice on TUI open

The daily maintenance sweep writes `persistent_warnings` (any Warn/Fail
doctor check that appeared in two consecutive sweeps) to the latest
`doctor_runs` row. On TUI startup, `App::new` queries that row and, if
`persistent_warnings` is non-empty, pushes a one-shot `System` cell into
the transcript right after the opening card:

```
⚠ doctor: 2 persistent warning(s) — run `borg doctor` for details
  • Memory:Index staleness
  • Skills:tamper audit
```

This is **intentionally not a persistent banner**. It renders once at
session start and scrolls away with any subsequent activity. More than
three warnings collapse into a "… and N more" footer line so chronic
neglect doesn't turn the startup notice into a wall of text. The report
is always available in full via `borg doctor` or by querying
`doctor_runs.report_json` directly.

Implementation: `load_persistent_warning_notice` /
`persistent_warning_notice_from_warnings` in
`crates/interface/src/tui/app/mod.rs`.

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
