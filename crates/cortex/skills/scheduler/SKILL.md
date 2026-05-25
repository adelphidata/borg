---
name: scheduler
description: "Schedule reminders, one-shot tasks, and recurring jobs via the schedule tool"
category: core
triggers:
  - "remind me"
  - "schedule this"
  - "every weekday"
  - "run this later"
---

# Scheduler

Use the `schedule` tool for any deferred or recurring work. Never use
`run_shell` with `sleep`, `at`, or background loops.

## Pick `schedule_type`

- **Specific time** ("tomorrow 9am", "in 30 min") → `once` with ISO-8601
  `schedule_expr`. Compute the timestamp from current time in the system
  prompt. **Never use `interval` for one-shots** — `interval` repeats forever.
- **Fixed duration** ("every 2h") → `interval`, `schedule_expr="2h"`.
- **Calendar recurrence** ("weekdays 8am") → `cron`, 5-field.

ISO-8601: `2026-04-22T09:00:00Z` (UTC), `…-07:00` (offset), or naive
`2026-04-22T09:00:00` (local TZ). Past timestamps are rejected.

For gateway-originated chats, set `delivery_channel="origin"` so the
reminder fires back into the same thread; omit for local TUI/REPL.

## Examples

One-shot reminder:

```json
{
  "action": "create", "type": "prompt",
  "name": "Stretch reminder",
  "prompt": "Remind the user to stretch.",
  "schedule_type": "once",
  "schedule_expr": "2026-04-21T14:30:00",
  "delivery_channel": "origin"
}
```

Cron (weekdays 8am):

```json
{
  "action": "create", "type": "prompt",
  "name": "Weekday calendar check",
  "prompt": "Summarize today's calendar events.",
  "schedule_type": "cron",
  "schedule_expr": "0 8 * * 1-5"
}
```

Shell cron:

```json
{
  "action": "create", "type": "command",
  "name": "Nightly backup",
  "command": "rsync -a ~/data /backups/",
  "schedule": "0 3 * * *"
}
```

## Other actions

`list`, `get`, `runs`, `pause`, `resume`, `cancel`, `delete`, `run_now` —
all take an `id`.
