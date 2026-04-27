---
name: scheduler
description: "Schedule reminders, one-shot tasks, and recurring jobs via the schedule tool"
category: core
---

# Scheduler Skill

Use the `schedule` tool any time the user asks to be reminded, wants a task to run later, or sets up recurring automation. This is the **only correct way** to defer work — never use `run_shell` with `sleep`, `at`, or background loops.

## Decision rule (must follow)

- **One-shot at a specific time** (e.g. "remind me tomorrow at 9am", "in 30 minutes") → `schedule_type="once"` with an ISO-8601 `schedule_expr`. Compute the timestamp yourself from the current time in the system prompt.
- **Repeats every fixed duration** (e.g. "every 2 hours") → `schedule_type="interval"`, `schedule_expr="2h"`.
- **Calendar-style recurrence** (e.g. "every weekday at 8am") → `schedule_type="cron"`, 5-field expression.

**Critical:** one-shot reminders always use `once`, never `interval`. `interval` repeats forever.

## Examples

### "Remind me tomorrow at 9am to go to the dr appointment"

Current time is in the system prompt. Add 1 day, set hour to 9, format as ISO-8601.

```json
{
  "action": "create",
  "type": "prompt",
  "name": "Reminder: dr appointment",
  "prompt": "Remind the user: go to the dr appointment.",
  "schedule_type": "once",
  "schedule_expr": "2026-04-22T09:00:00",
  "delivery_channel": "origin"
}
```

### "Ping me in 30 minutes to stretch"

Compute now + 30m as ISO-8601.

```json
{
  "action": "create",
  "type": "prompt",
  "name": "Stretch reminder",
  "prompt": "Remind the user to stretch.",
  "schedule_type": "once",
  "schedule_expr": "2026-04-21T14:30:00",
  "delivery_channel": "origin"
}
```

### "Every weekday at 8am check my calendar"

```json
{
  "action": "create",
  "type": "prompt",
  "name": "Weekday calendar check",
  "prompt": "Summarize today's calendar events for the user.",
  "schedule_type": "cron",
  "schedule_expr": "0 8 * * 1-5"
}
```

### "Every 2 hours run the sync"

```json
{
  "action": "create",
  "type": "prompt",
  "name": "Sync every 2h",
  "prompt": "Run the sync task.",
  "schedule_type": "interval",
  "schedule_expr": "2h"
}
```

### Shell cron

```json
{
  "action": "create",
  "type": "command",
  "name": "Nightly backup",
  "command": "rsync -a ~/data /backups/",
  "schedule": "0 3 * * *"
}
```

## ISO-8601 format

- With timezone: `2026-04-22T09:00:00Z` (UTC) or `2026-04-22T09:00:00-07:00`.
- Naive (no timezone offset): `2026-04-22T09:00:00` or `2026-04-22 09:00:00` — interpreted in the **system's local timezone**.
- Past timestamps are rejected.

## Delivery

When the user is chatting through a gateway channel (Telegram, Slack, Discord, etc.), set `delivery_channel="origin"` so the reminder fires back into the same chat/thread. Omit for local TUI/REPL use.

## Other actions

- `list` — show all scheduled jobs.
- `get` / `runs` — inspect a specific job by `id`.
- `pause` / `resume` / `cancel` / `delete` — manage lifecycle.
- `run_now` — fire a job immediately, ignoring its schedule.
