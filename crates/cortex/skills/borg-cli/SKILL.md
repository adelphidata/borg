---
name: borg-cli
description: "Drive the borg CLI to self-configure and diagnose: settings, skills, memory, tasks, channels, status, recent failures."
category: core
triggers:
  - "configure borg"
  - "change a setting"
  - "enable a skill"
  - "disable a skill"
  - "remember this"
  - "schedule a task"
  - "borg status"
  - "what failed"
  - "what was the last session"
---

# Borg CLI

Run any of these via `run_shell` to self-configure Borg. The daemon auto-starts; settings and DB writes apply immediately to the running agent.

## Settings (every config key, two-tier: DB > defaults)

```bash
borg settings                    # list all keys with effective value + source
borg settings get <key>          # one key
borg settings set <key> <value>  # write DB override
borg settings unset <key>        # drop override, revert to default
```

Examples: `borg settings set temperature 0.7`, `borg settings set conversation.collaboration_mode execute`, `borg settings set heartbeat.interval 30m`. For per-skill toggles: `borg settings set skills.entries.<name>.enabled false`.

## Skills

```bash
borg skills                      # list (name, category, source, enabled)
borg skills show <name>          # print SKILL.md body
borg skills enable <name>
borg skills disable <name>
```

User skills live at `~/.borg/skills/<name>/SKILL.md` and override built-ins. Use the `skill-creator` skill to author new ones.

## Memory

```bash
borg memory list [--scope <s>]              # default scope: global
borg memory read <scope> <name>
borg memory write <scope> <name> --content <text>
borg memory write <scope> <name> --file <path>
borg memory write <scope> <name> --content <text> --append
borg memory search <query> [--scope <s>] [--top-k 10]
borg memory delete <scope> <name>
```

Scope conventions: `global`, `project:<id>`. Names are stored without `.md`. 20k-token cap per entry; injection-scanned on write.

## Tasks & Cron

```bash
borg tasks                       # list scheduled agent tasks
borg tasks create -n <name> -p <prompt> -s "<cron|interval>" [-t cron|interval]
borg tasks delete|pause|resume|run|status|runs <id>
borg cron list|add|remove        # shell-command cron jobs
```

## Diagnose recent activity

Use these when asked "what failed", "what's flagged", "what's the last session". Prefer CLI; sqlite for cross-task lookups.

```bash
borg tasks                       # status flags failing tasks
borg tasks runs <id>             # last N runs for one task
# Cross-task failures (last 24h):
sqlite3 ~/.borg/borg.db "SELECT st.name, tr.error, datetime(tr.started_at,'unixepoch','localtime') FROM task_runs tr JOIN scheduled_tasks st ON st.id=tr.task_id WHERE tr.status='failed' AND tr.started_at>=strftime('%s','now','-24 hours') ORDER BY tr.started_at DESC LIMIT 10;"
borg doctor                      # current findings
borg sessions list --limit 5
```

## Channels & Pairing

```bash
borg add <channel>               # interactive setup (telegram, slack, discord, ...)
borg remove <channel>
borg plugins                     # list installed integrations
borg pairing list|approve <code>|approved|revoke
```

## Status, Health, Sessions

```bash
borg status [history|xp|evolution|card]
borg bond [history]
borg usage                       # token + cost breakdown
borg doctor                      # diagnostics
borg doctor squelches            # list silenced findings
borg doctor squelch <category> <name> [--ttl-days N]
borg doctor unsquelch <category> <name>
borg heal                        # one-shot self-heal (logs, embeddings, stalled tasks)
borg poke                        # trigger heartbeat now
borg dream                       # run memory consolidation
borg sessions list [--limit N]
borg sessions show <id>
borg --resume <id>               # resume a session in TUI
borg export <id> [-f json|csv|txt]
```

## Service, Update, Backup

```bash
borg service install [--secure] | uninstall | status | restart
borg stop | restart              # daemon control
borg update [--check] [--prerelease]
borg backup create | verify <file> | restore <file> [--force]
```

## Mode

```bash
borg ask "<prompt>" [--mode default|execute|plan]
borg cancel [--session <id>]
borg away ["<msg>"] | borg available
```

## Tips

- Setting names are stable; run `borg settings` (no args) to discover keys.
- `borg settings set` validates the value; an invalid key/value returns non-zero.
- Most commands talk to the running daemon; if it's not up, `borg service status` and `borg service restart` recover it.
- In Plan mode, `run_shell` is blocked, so `borg ...` self-config will not execute. Switch to execute mode first (`borg settings set conversation.collaboration_mode execute`).
