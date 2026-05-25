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

Run via `run_shell`. Settings/DB writes apply live to the running daemon.
Plan mode blocks `run_shell` — switch with
`borg settings set conversation.collaboration_mode execute` first.

## Settings (DB > defaults)

```bash
borg settings                    # all keys + effective value + source
borg settings get|set|unset <key> [<value>]
```

Examples: `borg settings set temperature 0.7`,
`borg settings set heartbeat.interval 30m`,
`borg settings set skills.entries.<name>.enabled false`.

## Skills

```bash
borg skills                      # list (name, category, source, enabled)
borg skills show <name>
borg skills enable|disable <name>
```

User overrides at `~/.borg/skills/<name>/SKILL.md`. Author with the
`skill-creator` skill.

## Memory

```bash
borg memory list [--scope <s>]
borg memory read <scope> <name>
borg memory write <scope> <name> --content <text> [--append]   # or --file <path>
borg memory search <query> [--scope <s>] [--top-k 10]
borg memory delete <scope> <name>
```

Scopes: `global`, `project:<id>`. Names stored without `.md`. 20k-token cap,
injection-scanned.

## Tasks & cron

```bash
borg tasks                       # list (status flags failing)
borg tasks create -n <name> -p <prompt> -s "<cron|interval>" [-t cron|interval]
borg tasks delete|pause|resume|run|status|runs <id>
borg cron list|add|remove        # shell-command jobs
```

## Diagnose

```bash
borg tasks runs <id>             # last N runs
borg doctor                      # findings
borg sessions list --limit 5
# cross-task failures (24h):
sqlite3 ~/.borg/borg.db "SELECT st.name, tr.error, datetime(tr.started_at,'unixepoch','localtime') \
  FROM task_runs tr JOIN scheduled_tasks st ON st.id=tr.task_id \
  WHERE tr.status='failed' AND tr.started_at>=strftime('%s','now','-24 hours') \
  ORDER BY tr.started_at DESC LIMIT 10;"
```

## Channels & pairing

```bash
borg add|remove <channel>        # telegram, slack, discord, ...
borg plugins
borg pairing list|approve <code>|approved|revoke
```

## Status / health / sessions

```bash
borg status [history|xp|evolution|card]
borg bond [history]
borg usage
borg doctor squelches | squelch <cat> <name> [--ttl-days N] | unsquelch <cat> <name>
borg heal                        # one-shot self-heal
borg poke                        # heartbeat now
borg dream                       # memory consolidation
borg sessions list|show <id>
borg --resume <id>
borg export <id> [-f json|csv|txt]
```

## Service / update / backup

```bash
borg service install [--secure] | uninstall | status | restart
borg stop | restart
borg update [--check] [--prerelease]
borg backup create | verify <file> | restore <file> [--force]
```

## Ask / mode

```bash
borg ask "<prompt>" [--mode default|execute|plan]
borg cancel [--session <id>]
borg away ["<msg>"] | borg available
```
