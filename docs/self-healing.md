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
7. **Stalled-task scan.** Recomputes `next_run` for any recurring
   scheduled task whose fire time has drifted more than one hour into
   the past (see below). When more than
   `CLOCK_JUMP_AGGREGATE_THRESHOLD` (5) tasks look stalled in a single
   sweep — the usual signature of a laptop waking from long sleep —
   the audit trail collapses into **one** aggregate `task_runs` row
   (`status='missed'`) instead of N near-identical ones. `next_run`
   is still recomputed for every task; only the audit entry is
   aggregated. `HealReport.aggregated` flags the case for the daemon
   log.
8. **Wedged-run recovery.** Marks any `task_runs` row stuck in
   `status='running'` past its timeout as `failed`, using the task's own
   `timeout_ms` when present and the 1-hour default grace otherwise.
9. **Remediation registry.** Any doctor issue marked `AutoSafe` is
    eligible for one bounded repair attempt during this sweep. Current
    safe remediations are recreating runtime subdirs, seeding
    `global/INDEX`, and repairing stale/missing memory embeddings and
    chunk indexes.
10. **Issue lifecycle update.** Warn/Fail checks are written into
    `maintenance_issue_state`, which tracks `first_seen_at`,
    `last_seen_at`, `consecutive_count`, and the last auto-fix attempt.
    `MaintenanceReport.persistent_warnings` is derived from that table:
    only still-open issues with `consecutive_count >= 2` are surfaced as
    persistent warnings. Single-run flukes do not escalate. Findings the
    user has squelched via `/doctor squelch` (see below) are filtered
    out here so they do not surface.
11. **Doctor-squelch expiry.** Removes rows from `doctor_squelches`
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
| `maintenance.security_squelch_ttl_days` | `30` | How long a `/doctor` squelch persists before it auto-expires and the warning resumes. |
| `maintenance.vacuum_interval_days` | `7` | Minimum days between full `VACUUM` rewrites. WAL checkpoint runs every sweep regardless. |
| `maintenance.heal_action_budget` | `1000` | Cap on mutating self-heal actions per sweep before short-circuiting. Backstop for runaway repair loops. |

All eleven are wired through `/settings` under the **Maintenance**
section.

## WAL checkpoint and conditional VACUUM

Each sweep, after the heavy deletes (activity-log prune, embedding
cache, retention purge, stalled-task heal, wedged-run recovery), the
maintenance task runs:

1. `PRAGMA wal_checkpoint(TRUNCATE)` — every sweep. Cheap (microseconds
   when the WAL is small). Returns the page count to
   `MaintenanceReport.wal_checkpoint_pages_reclaimed`. `-1` is the
   sentinel for a checkpoint that failed (e.g. an in-memory test DB);
   any non-negative value is a real count.

2. `PRAGMA incremental_vacuum` — every sweep. Complements the
   `auto_vacuum=INCREMENTAL` set at connection open by actually
   reclaiming pages flagged by recent deletes.

3. **Conditional full** `VACUUM` — only when **both**:
   - `now - last_vacuum_at >= maintenance.vacuum_interval_days` (default
     7), AND
   - `freelist_count / page_count > 0.15` OR the freelist exceeds 64 MB
     of waste.

   Full VACUUM locks the DB and rewrites the file, so we run it only
   when fragmentation is real. `last_vacuum_at` lives in the `meta`
   table under key `maintenance.last_vacuum_at` and is updated only on
   successful completion.

Skipped VACUUM is a no-op (does not touch `last_vacuum_at`). Failed
checkpoint logs a warn and continues — never aborts the sweep.

Inspect telemetry:

```sql
SELECT ran_at, json_extract(report_json, '$.vacuum_ran') AS vacuumed,
       json_extract(report_json, '$.vacuum_bytes_reclaimed') AS bytes_reclaimed,
       json_extract(report_json, '$.wal_checkpoint_pages_reclaimed') AS wal_pages
FROM doctor_runs
ORDER BY ran_at DESC
LIMIT 10;
```

## Healing action budget

Maintenance steps that mutate state (delete rows, update task statuses,
repair embeddings) charge the `BudgetTracker`. Once
`maintenance.heal_action_budget` is exhausted, every subsequent
mutating step short-circuits and `MaintenanceReport.budget_exhausted`
is set true.

**Why default 1000?** A healthy 30-day install processes a few hundred
mutations per sweep. 1000 is the threshold at which "something is
genuinely runaway" is the most likely explanation — a corrupted row
that keeps re-triggering repair, or weeks of unprocessed backlog after
an outage. Defaulting lower (the original design suggested 100) would
false-positive on normal sweeps after a busy week.

What's counted (per row, summed across all listed steps):

- `activity_rows_deleted`
- `embeddings_pruned`
- `squelches_expired`
- `messages_purged`
- `audit_events_tombstoned`
- `stalled_tasks_healed`
- `wedged_runs_recovered`
- `embeddings_repaired_files`

What's **not** counted: log file deletions (filesystem, separate
budget via retention), log truncation bytes (one aggregate op),
embedding repair *chunks* (already capped by
`embedding_repair_max_chunks`).

Behavior on exhaustion:

1. The current step finishes (the gate is checked between steps, not
   mid-step).
2. All later steps short-circuit.
3. One `activity_log` row is written: `level=warn`, `category=maintenance`.
4. An issue is observed under key `Maintenance:heal action budget` —
   the existing persistent-warning lifecycle promotes it to a doctor
   `Warn` after two consecutive exhausts.

Inspect:

```sql
SELECT level, category, message FROM activity_log
WHERE category = 'maintenance'
ORDER BY created_at DESC LIMIT 20;
```

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

## Subprocess process registry

Every agent-initiated subprocess — `run_shell` children, the headless
`browser` (Chrome), hook scripts, scheduled command tasks, and
`pre_run_command` preambles — is registered in a daemon-side
`ProcessRegistry` at spawn time. The registry stores PID, command,
owning session, kind, and start time, and tracks lifecycle transitions
(Running → Exited / TimedOut / Killed / Dead) via an RAII `ProcessGuard`
so spawn sites cannot forget to deregister.

Short-lived internal utilities (`git`, `sips`, `security`, `launchctl`)
are intentionally **not** tracked — they finish in milliseconds and
would crowd out the signal we actually want, which is "what's still
running that shouldn't be."

Surfaces:

- `borg status` / `/status` — one section listing running children
  with age and kind. Hidden when empty.
- `borg doctor` / `/doctor` — `Subprocesses:Long-running children`
  check: Pass when all entries are younger than
  `process_registry.orphan_warn_secs` (default 1h), Warn when any
  exceed it, Fail at `process_registry.orphan_fail_secs` (default 4h).
  Remediation is `Manual` — Borg surfaces the orphan; the operator
  decides whether to kill it. Squelchable via
  `/doctor squelch Subprocesses:Long-running children`.

Each daily maintenance sweep runs three registry steps after the
embedding-cache prune:

1. **Liveness reap** — sends `signal(0)` to every Running entry's PID.
   Any entry whose PID has vanished (daemon crash mid-spawn, kernel
   reaped the process before the guard could fire) is marked `Dead`
   and moved to the finished queue. Reported as
   `MaintenanceReport.subprocess_dead_reaped`.
2. **Auto-reap** — entries older than `process_registry.auto_reap_secs`
   (default 24h) are SIGTERM'd, then SIGKILL'd after
   `process_registry.sigterm_grace_secs` (default 5s). The original
   command is captured to `activity_log` at `category=process_registry`
   before the kill so post-mortem is always possible. Reported as
   `MaintenanceReport.subprocess_auto_killed`. Set
   `process_registry.auto_reap_secs = 0` to disable auto-reap entirely.
3. **Finished-queue prune** — finished entries older than
   `process_registry.finished_ttl_secs` (default 30 min) are evicted,
   and the queue is bounded by `process_registry.max_finished_entries`
   (default 256) with LRU eviction. Reported as
   `MaintenanceReport.subprocess_finished_pruned`.

All three actions charge the `heal_action_budget` so a registry that's
somehow grown unbounded cannot turn a sweep into a multi-second hang.

### Auto-reap whitelist

Three layers prevent legitimate long-running workloads from being
killed at 24h:

1. **Kind exemption (hard-coded).** `Browser` (headless Chrome) is
   never auto-killed. The browser session is meant to outlive any
   single tool call, and there is no useful "stuck Chrome" heuristic
   that doesn't also break correct usage. Counted as
   `subprocess_auto_exempted`, not killed.
2. **Pattern allowlist.** `process_registry.never_kill_patterns`
   (default `["cargo build*", "cargo test*", "cargo run*",
   "cargo clippy*", "make*", "pytest*", "npm run*", "bun *"]`) is
   glob-matched against the command string. Add host-specific entries
   with `borg settings set process_registry.never_kill_patterns ...`.
3. **Per-process pin.** When `/doctor` shows a Warn for a workload you
   know is legitimately running long, pin it from the TUI subprocess
   view: cursor onto the row and press `p` to toggle the pin (same
   inline-toggle pattern as `/settings`). Pinned entries are exempt
   from auto-reap and contribute to neither Warn nor Fail counts. The
   pin clears automatically when the process exits — there is no need
   to unpin.

If the same command is auto-reaped on two consecutive sweeps,
maintenance surfaces a persistent warning at `Subprocesses:repeated
auto-reap` with the hint `add to process_registry.never_kill_patterns`
— a "you keep killing this; whitelist it" nudge.

| Key | Default | Meaning |
|-----|---------|---------|
| `process_registry.orphan_warn_secs` | `3600` | Age past which a running subprocess triggers a doctor Warn. |
| `process_registry.orphan_fail_secs` | `14400` | Age past which a running subprocess triggers a doctor Fail. |
| `process_registry.auto_reap_secs` | `86400` | Age past which the daily sweep SIGTERMs+SIGKILLs the subprocess. `0` disables auto-reap. |
| `process_registry.sigterm_grace_secs` | `5` | Seconds between SIGTERM and SIGKILL during auto-reap. |
| `process_registry.never_kill_patterns` | (8 build/test globs) | Glob patterns exempt from auto-reap. |
| `process_registry.finished_ttl_secs` | `1800` | How long completed entries linger before the sweep evicts them. |
| `process_registry.max_finished_entries` | `256` | Hard cap on the finished queue; oldest evicted first. |

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
