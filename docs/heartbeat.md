# Heartbeat

The heartbeat system enables proactive check-ins -- the agent can reach out to you at regular intervals without being prompted.

> See also: [Self-Healing](./self-healing.md) for missed-run detection on scheduled tasks and the daily maintenance sweep.

## How it works

1. A `HeartbeatScheduler` runs inside `borgd` as a separate tokio task
2. At each interval tick (or cron fire), the agent sends a prompt to the LLM with its personality, memory context, and HEARTBEAT.md checklist
3. If the LLM has something useful to say, it responds; otherwise it returns an empty message
4. `borgd` publishes heartbeat lifecycle events; the TUI subscribes and renders heartbeat messages in cyan as a `[heartbeat]` prefix
5. Duplicate and empty responses are suppressed; heartbeats also skip cleanly (`skip_reason="no_llm_provider"`) when `llm.provider` is unset, so the onboarding-race window before the ConfigWatcher's 3 s poll catches up never surfaces as a `[heartbeat error: …]` toast
6. Responses can be delivered to messaging channels (Telegram, Slack, Discord)

## Configuration

```sh
borg settings set heartbeat.interval "30m"
borg settings set heartbeat.cron "0 */30 * * * *"       # optional, overrides interval
borg settings set heartbeat.quiet_hours_start "00:00"
borg settings set heartbeat.quiet_hours_end "06:00"
borg settings set heartbeat.channels '["telegram"]'     # empty = local TUI/transcript only
```

### Interval format

| Format | Example | Duration |
|--------|---------|----------|
| Minutes | `"30m"` | 30 minutes |
| Hours | `"2h"` | 2 hours |
| Seconds | `"45s"` | 45 seconds |
| Bare number | `"120"` | 120 seconds |

Minimum interval is 60 seconds (enforced to prevent API waste). The
floor is configurable via `heartbeat.min_interval_secs` (default `60`,
hard floor `1`). Leave at `60` in production — sub-minute firings exist
to let the smorgasborg eval suite exercise real scheduler timing
without burning ≥60s of wall time per assertion. The same floor is
applied to cron expressions: a cron that fires faster than
`min_interval_secs` falls back to interval mode.

### Cron scheduling

The `cron` field accepts standard cron expressions and overrides `interval` when set. Example: `"0 */30 * * * *"` fires every 30 minutes.

### Quiet hours

Quiet hours prevent heartbeats during specified time ranges. The range correctly spans midnight -- setting `"00:00"` to `"06:00"` suppresses heartbeats overnight.

Quiet hours are interpreted in the timezone configured in `[user] timezone` (IANA string, e.g., `America/New_York`). If no timezone is set, the system local time is used.

`user.timezone` is auto-detected from the host on first onboarding (or first daemon start for pre-existing installs); afterward, if a `write_memory` call mentions a common timezone (`PST`, `EST`, `Europe/Berlin`, …) and quiet-hour times are still at their defaults (`00:00`–`06:00`), the timezone slides to match — once the user changes either default time, the slide is disabled.

If quiet hours are not configured, heartbeats fire at every interval.

### Channel delivery

When `channels` is set (e.g., `["telegram"]`), heartbeat responses are delivered to the owner's `sender_id` from the `approved_senders` table. This requires the gateway to be running and the owner to have paired with the channel.

When `channels` is empty, heartbeats still run. They are simply rendered locally in the TUI transcript instead of being pushed to an external messaging provider.

## Session-start greetings

Opening the TUI can trigger a proactive `session_start` heartbeat even if no messaging provider is configured.

- It uses a dedicated session-start prompt rather than the generic scheduled heartbeat prompt.
- It respects `heartbeat.session_start_enabled` and `heartbeat.session_start_throttle_minutes`.
- It bypasses quiet hours because opening the TUI is an explicit user action.
- If no messaging channels are configured, Borg can proactively suggest installing one from `/plugins`.

### HEARTBEAT.md checklist

Create an optional `~/.borg/HEARTBEAT.md` file with a checklist for the agent to follow during heartbeat turns. For example:

```markdown
- Check Gmail for unread messages
- Check Google Calendar for upcoming events
- Review any pending GitHub notifications
```

This file is injected into the heartbeat agent turn so the agent can proactively check email, calendar, etc.

## Deduplication

If the LLM responds with the exact same message as the previous heartbeat, it's suppressed. This prevents repetitive notifications when nothing has changed.

## Poke command

Trigger an immediate heartbeat check-in at any time:

```sh
borg poke
```

From the TUI, use `/poke`.

This sends a poke signal to the daemon, triggering a scheduled heartbeat that bypasses quiet hours. The CLI goes through the daemon gRPC API and fails if borgd is not running. The gateway's chat `/poke` command reaches the same runner through the loopback-only `/internal/poke` endpoint.

## Daemon mode

For running heartbeat check-ins and scheduled tasks without the interactive TUI, use daemon mode:

```sh
borg daemon
```

The daemon runs the heartbeat scheduler alongside the gateway server. It executes heartbeat check-ins, session-start greetings, scheduled tasks, and messaging webhooks. Combine with `borg service install` to run it as a system service (launchd on macOS, systemd on Linux).

### Tick lock

The dispatcher holds a non-blocking `flock` on `~/.borg/scheduler.tick.lock` (`crates/borgd/src/tick_lock.rs`) across the claim+dispatch phase of each iteration. The lock is acquired after maintenance work (drift recovery, stalled-task scan) and dropped before the next `interval.tick()` await — task execution itself runs unlocked off the dispatcher loop.

Three things this guards, beyond what `pidlock.rs` (single daemon) and SQLite's `BEGIN IMMEDIATE` (single writer) already cover:

- **Out-of-process tick callers** — a future manual CLI tick, maintenance hook, or ACP trigger calling `claim_due_tasks` while the daemon is up.
- **Re-entrant in-process iteration** — if a tick stalls past `DAEMON_LOOP_INTERVAL`, the next `interval.tick()` would otherwise fire immediately and re-enter the claim arm before the previous one finished.
- **Architectural clarity** — "no two ticks run concurrently" is now an observable property gated by a named primitive, not an emergent one.

On collision, the dispatcher logs at `debug` and skips the tick. The next interval (60 s) retries naturally because `next_run` is only advanced inside the locked section.

## Heartbeat prompt

Each heartbeat sends the agent's personality (IDENTITY.md), memory context, HEARTBEAT.md checklist (if present), and current time, along with instructions to:
- Follow the HEARTBEAT.md checklist if present
- Share something useful, timely, or caring if appropriate
- Respond with an empty message if there's nothing meaningful to say
- Keep it brief -- one or two sentences max

## Proactive nudges (augmenters)

Beyond the base prompt, every heartbeat tick runs a list of *augmenter*
checks. Each check inspects current state (config, DB, files); when its
condition is true it appends a directive to the heartbeat user message
inside a `<proactive_nudges>` block. The agent rewrites the directive in
its own voice. Each augmenter is rate-limited via the `meta` table.

Source: `crates/borgd/src/heartbeat_augmenters.rs`.

| ID | Cooldown | Fires when | Source of truth |
|---|---|---|---|
| `no_channels_configured` | 24h | No native messaging channel set up | Config |
| `critical_host_security` | 48h | `borg doctor` host checks fail (firewall, risky ports, SSH config) | `host_audit` |
| `task_health` | 6h | Tasks carry `last_error` or there are failed task_runs in the last 24h | DB: `scheduled_tasks`, `task_runs` |
| `budget_threshold` | 6h | Monthly token usage crosses 75 / 90 / 100% (each bucket fires once per month) | DB: `token_usage` + `budget.monthly_token_limit` |
| `identity_unpersonalized` | 7d | `~/.borg/IDENTITY.md` is missing, equals the bare default, or still contains template placeholders | File |
| `gateway_unrecoverable` | 24h | Orchestrator supervisor abandoned the gateway after `GATEWAY_MAX_CRASH_RESPAWNS` AND the daily self-healing recovery probe couldn't bring it back | DB: `meta.gateway_unrecoverable` |

Session-start greetings only run a focused subset — currently
`no_channels_configured` and `identity_unpersonalized` — so opening the
TUI nudges the user toward setup without dumping every operational
warning at once.

### Adding a new augmenter

Append one entry to `AUGMENTERS` in
`crates/borgd/src/heartbeat_augmenters.rs` — the module-level docs walk
through the full pattern. Future ideas worth considering:

- `prompt_cache_hit_rate_low` — Anthropic cache hit rate degrading.
- `memory_index_stale` — `global/INDEX` last-rebuilt timestamp older
  than N days.
- `disk_space_low` — `~/.borg/` partition under 10% free.
- `daemon_restart_loop` — daemon respawned more than N times in the
  last hour.

### Exceptions and TODOs

- **`request_user_input` is multiple-choice only** (max 3 questions,
  2-4 options each, with a UI-appended "Other" + Tab notes field for
  free text). Open-ended questions need to be asked conversationally
  in chat *after* the structured batch. The `identity_unpersonalized`
  nudge uses this hybrid pattern — see the directive in
  `heartbeat_augmenters.rs`.
- **`gateway_unrecoverable` depends on a 1-hour grace period** so the
  augmenter doesn't race the daily 02:00 maintenance pass. Three
  layers of defense, in order:
  1. Orchestrator's respawn supervisor — up to
     `GATEWAY_MAX_CRASH_RESPAWNS` restarts with exponential backoff
     (`crates/borgd/src/orchestrator.rs`). On abandonment it writes
     `meta.gateway_unrecoverable=1` plus a timestamp.
  2. Daily maintenance probes the configured listen address; if the
     gateway is now answering, the flag clears (covers the "user
     restarted the daemon" case). Source:
     `cortex::maintenance::recover_gateway_if_abandoned`.
  3. Heartbeat augmenter — last resort. Outbound channel sends still
     work (native SDKs bypass the gateway), so the nudge reaches
     channels too; inbound webhooks are dead, which is exactly what
     the user needs to be told. If you change the maintenance schedule
     also revisit the grace window.
- **`budget_threshold` layers on top of `budget.warning_threshold`**
  (the existing `BudgetEngine` hook in `crates/cortex/src/budgets/`).
  Both can fire when monthly usage crosses a threshold; the heartbeat
  one is conversational, the engine one is a system event. Each
  threshold (75 / 90 / 100) fires at most once per calendar month —
  the per-month meta key is
  `heartbeat_budget_last_bucket_{YYYY-MM}`. Re-firing for a *higher*
  threshold inside the same month is allowed (you'll hear about 75 →
  90 → 100), but the same bucket will not repeat.
