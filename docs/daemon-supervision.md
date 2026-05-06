# Daemon Subsystem Supervision

Borgd is a long-running Tokio host with several independent
subsystems: gateway, scheduled-task dispatcher, iMessage monitor,
heartbeat trigger loop, budget tick. Default `tokio::spawn` swallows
panics — a single bad event in any one of them turns the daemon into
a half-working zombie that looks alive from the OS but stops doing
the thing the user installed it for.

The supervisor module (`crates/borgd/src/supervisor.rs`) wraps each
long-running task with three properties: panic catching, automatic
respawn with exponential backoff, and a degraded-state cap that gives
up gracefully after persistent failure.

## What's supervised today

| Subsystem | Module | `max_restarts` | `base_backoff` | `max_backoff` | Notes |
|-----------|--------|----------------|----------------|---------------|-------|
| `gateway` | `orchestrator.rs` | 5 | `GATEWAY_RESPAWN_BASE_DELAY` | base × 16 | Migrated from prior ad-hoc loop. On degrade, calls `mark_gateway_unrecoverable` to set the heartbeat-augmenter meta keys. |
| `imessage-monitor` | `orchestrator.rs` (macOS only) | 5 | 1s | 5min | Skips silently when channel.toml is missing or Full Disk Access is denied. |
| `scheduler-dispatch` | `orchestrator.rs` | 5 | 1s | 5min | Wraps `scheduler::run`; restart on dispatcher panic or unexpected `Err` return. |

Not supervised:

- **Signal handler** — one-shot task, no respawn semantics.
- **Config-watcher loop** — driven by an external `watch::Sender`;
  restart wouldn't reconnect to the producer.
- **Telemetry sampler** — best-effort, panics already isolated by the
  gRPC layer.

The heartbeat trigger loop in `borgd::heartbeat::spawn` is bare
`tokio::spawn` today; the V52 consolidation moved scheduled heartbeats
into the unified scheduler, so the trigger loop only handles
admin-initiated triggers.

## How it works

```
spawn_supervised(cfg, cancel, db, factory) -> JoinHandle<()>
```

The factory closure is called once per (re)spawn. Each iteration:

1. Spawn the inner task wrapped in `AssertUnwindSafe(...).catch_unwind()`.
2. Await the join. Match on:
   - `Ok(Ok(Ok(())))` — clean exit. Stop supervising.
   - `Ok(Ok(Err(e)))` — task returned an error. Treat as restart.
   - `Ok(Err(panic))` — task panicked. Treat as restart.
   - `Err(JoinError::is_cancelled())` with the shared cancel set —
     clean shutdown.
   - `Err(JoinError::is_cancelled())` without the shared cancel — an
     external `AbortHandle` aborted the task (e.g. the watchdog
     firing on a stalled scheduler). Treat as restart.

3. Append the restart timestamp to a sliding window of length
   `restart_window`. Prune anything older than the window.
4. If the window length exceeds `max_restarts`:
   - Log `error`-level activity row.
   - Insert `maintenance_issue_state` row keyed `Subsystem:{name}` with
     status `fail`. The next maintenance sweep promotes this to a
     doctor `Fail` finding via the existing issue-lifecycle plumbing —
     no new doctor wiring required.
   - Call the optional `on_degraded` callback (gateway uses this to
     set the legacy unrecoverable meta keys).
   - Stop supervising.
5. Otherwise, exponential backoff (`base_backoff × 2^restart_count`,
   capped at `max_backoff`) and respawn.

## Degraded-state semantics

Once a subsystem hits degraded state:

- `borg doctor` shows it as a Fail finding under category `Subsystem`.
- `/heal` does **not** clear the degraded state — it requires an
  explicit operator restart of the daemon (the assumption is that
  whatever caused 5 panics in 10 minutes is unlikely to be fixed by
  retrying without diagnosis).
- The supervisor task itself exits cleanly. The subsystem stays down
  until the daemon restarts.

Inspect:

```sql
SELECT issue_key, last_status, first_seen_at, last_seen_at, consecutive_count
FROM maintenance_issue_state
WHERE issue_key LIKE 'Subsystem:%';
```

## The heartbeat-liveness watchdog

The supervisor catches panics that *escape* a task. It cannot detect a
task that's alive but not making progress — a deadlock inside an
awaited future, a hung DB query, async mutex contention. For that, the
watchdog (`spawn_liveness_watchdog`) reads a shared
`Arc<AtomicU64>` counter every 30 seconds and aborts the task when the
counter doesn't change for 5 minutes (10 polls).

The heartbeat scheduler is the canonical example of a loop with this
risk. It exposes `liveness_handle()` returning the counter; the
scheduler increments it on every iteration AND on every
`LIVENESS_TICK_MAX` (60s) forced wake. So a healthy scheduler bumps
the counter at least once per minute even during long cron sleeps.

Watchdog logic:

```rust
loop {
    sleep(30s);
    if liveness == last_liveness {
        stale_polls += 1;
        if stale_polls >= 10 {
            abort.abort();
            return;
        }
    } else {
        stale_polls = 0;
        last_liveness = liveness;
    }
}
```

The aborted task surfaces to the supervisor as `JoinError::is_cancelled()`
without the shared cancel token set — the supervisor treats this as a
restart (counts toward the degraded-state cap so persistent stalls
also escalate cleanly).

Three layers in order of preference:

1. **Defensive iteration** (`heartbeat::scheduler`) — each loop
   iteration body is wrapped in `catch_unwind`; a panic in cron
   parsing or any descendant logs and the loop continues.
2. **Stall watchdog** — catches alive-but-not-iterating.
3. **Subsystem supervisor** — catches everything else.

## Activity-log audit trail

Every restart writes one row:

```
level   = warn (Err return) | error (panic) | warn (external abort)
category = subsystem
message = "{name}: {reason}"
```

Degraded state writes one additional row:

```
level   = error
category = subsystem
message = "{name}: degraded after N restarts in <window>; supervisor giving up"
```

Inspect:

```sql
SELECT level, category, message, datetime(created_at, 'unixepoch') AS at
FROM activity_log
WHERE category = 'subsystem'
ORDER BY created_at DESC
LIMIT 50;
```

## Adding a new supervised subsystem

```rust
use crate::supervisor::{spawn_supervised, SupervisorConfig};

let cfg = SupervisorConfig::new("my-subsystem");
spawn_supervised(cfg, shutdown.clone(), supervisor_db.clone(), move || {
    // Each (re)spawn calls this closure.
    let state = state.clone();
    async move {
        my_subsystem::run(state).await
            .map_err(|e| anyhow::anyhow!("my-subsystem: {e}"))
    }
});
```

The factory must return `Future<Output = Result<(), E>>` — `Ok(())` is
a clean exit, `Err` triggers restart. The closure must be `Fn` (called
once per restart) and `Send + Sync + 'static`.

For optional degraded-state behavior:

```rust
let cfg = SupervisorConfig::new("my-subsystem")
    .with_on_degraded(|| {
        my_subsystem::mark_unrecoverable();
    });
```

## Forbidden: bare `tokio::spawn` for new long-running daemon tasks

Adding a fresh `tokio::spawn(async move { loop { ... } })` to `borgd`
is now a regression. New long-running subsystems must go through
`spawn_supervised`. Short-lived tasks (one-shot signal handlers,
single requests) are fine to spawn directly.
