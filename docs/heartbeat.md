# Heartbeat

The heartbeat system enables proactive check-ins -- the agent can reach out to you at regular intervals without being prompted.

> See also: [Self-Healing](./self-healing.md) for missed-run detection on scheduled tasks and the daily maintenance sweep.

## How it works

1. A `HeartbeatScheduler` runs as a separate tokio task alongside the TUI or daemon
2. At each interval tick (or cron fire), the agent sends a prompt to the LLM with its personality, memory context, and HEARTBEAT.md checklist
3. If the LLM has something useful to say, it responds; otherwise it returns an empty message
4. Heartbeat messages render in cyan as `[heartbeat]` prefix in the TUI
5. Duplicate and empty responses are suppressed
6. Responses can be delivered to messaging channels (Telegram, Slack, Discord)

## Configuration

```sh
borg settings set heartbeat.interval "30m"
borg settings set heartbeat.cron "0 */30 * * * *"       # optional, overrides interval
borg settings set heartbeat.quiet_hours_start "00:00"
borg settings set heartbeat.quiet_hours_end "06:00"
borg settings set heartbeat.channels '["telegram"]'     # empty = TUI only
```

### Interval format

| Format | Example | Duration |
|--------|---------|----------|
| Minutes | `"30m"` | 30 minutes |
| Hours | `"2h"` | 2 hours |
| Seconds | `"45s"` | 45 seconds |
| Bare number | `"120"` | 120 seconds |

Minimum interval is 60 seconds (enforced to prevent API waste).

### Cron scheduling

The `cron` field accepts standard cron expressions and overrides `interval` when set. Example: `"0 */30 * * * *"` fires every 30 minutes.

### Quiet hours

Quiet hours prevent heartbeats during specified time ranges. The range correctly spans midnight -- setting `"00:00"` to `"06:00"` suppresses heartbeats overnight.

Quiet hours are interpreted in the timezone configured in `[user] timezone` (IANA string, e.g., `America/New_York`). If no timezone is set, the system local time is used.

If quiet hours are not configured, heartbeats fire at every interval.

### Channel delivery

When `channels` is set (e.g., `["telegram"]`), heartbeat responses are delivered to the owner's `sender_id` from the `approved_senders` table. This requires the gateway to be running and the owner to have paired with the channel.

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

This sends a poke signal to the heartbeat scheduler, triggering a heartbeat that bypasses quiet hours. The CLI sends an HTTP POST to `/internal/poke` on the gateway; the TUI sends directly via channel when it owns the scheduler.

## Daemon mode

For running heartbeat check-ins and scheduled tasks without the interactive TUI, use daemon mode:

```sh
borg daemon
```

The daemon runs the heartbeat scheduler alongside the gateway server. It executes heartbeat check-ins, scheduled tasks, and handles messaging webhooks. Combine with `borg service install` to run it as a system service (launchd on macOS, systemd on Linux).

## Heartbeat prompt

Each heartbeat sends the agent's personality (IDENTITY.md), memory context, HEARTBEAT.md checklist (if present), and current time, along with instructions to:
- Follow the HEARTBEAT.md checklist if present
- Share something useful, timely, or caring if appropriate
- Respond with an empty message if there's nothing meaningful to say
- Keep it brief -- one or two sentences max
