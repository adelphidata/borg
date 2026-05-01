# Heartbeat

The heartbeat system enables proactive check-ins -- the agent can reach out to you at regular intervals without being prompted.

> See also: [Self-Healing](./self-healing.md) for missed-run detection on scheduled tasks and the daily maintenance sweep.

## How it works

1. A `HeartbeatScheduler` runs inside `borgd` as a separate tokio task
2. At each interval tick (or cron fire), the agent sends a prompt to the LLM with its personality, memory context, and HEARTBEAT.md checklist
3. If the LLM has something useful to say, it responds; otherwise it returns an empty message
4. `borgd` publishes heartbeat lifecycle events; the TUI subscribes and renders heartbeat messages in cyan as a `[heartbeat]` prefix
5. Duplicate and empty responses are suppressed
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

This sends a poke signal to the daemon, triggering a scheduled heartbeat that bypasses quiet hours. The CLI prefers the daemon gRPC API and falls back to the legacy HTTP `/internal/poke` gateway endpoint if needed.

## Daemon mode

For running heartbeat check-ins and scheduled tasks without the interactive TUI, use daemon mode:

```sh
borg daemon
```

The daemon runs the heartbeat scheduler alongside the gateway server. It executes heartbeat check-ins, session-start greetings, scheduled tasks, and messaging webhooks. Combine with `borg service install` to run it as a system service (launchd on macOS, systemd on Linux).

## Heartbeat prompt

Each heartbeat sends the agent's personality (IDENTITY.md), memory context, HEARTBEAT.md checklist (if present), and current time, along with instructions to:
- Follow the HEARTBEAT.md checklist if present
- Share something useful, timely, or caring if appropriate
- Respond with an empty message if there's nothing meaningful to say
- Keep it brief -- one or two sentences max
