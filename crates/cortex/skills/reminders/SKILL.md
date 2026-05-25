---
name: reminders
description: "Manage native macOS Reminders via reminders-cli"
requires:
  bins: ["reminders"]
---

# Reminders

macOS-only. `brew install keith/formulae/reminders-cli`. First run prompts for
TCC access (System Settings → Privacy & Security → Reminders). Changes sync
to other Apple devices via iCloud.

**Indices shift** after complete/delete — re-`show` before referencing one.
Due dates take natural language (`"Friday 5pm"`, `"tomorrow"`, `"in 2 hours"`).

## Read

```bash
reminders show-lists                          # all lists
reminders show "Inbox" [--include-completed]
reminders show-all
# Add --format json to most subcommands for parseable output.
```

## Add

```bash
reminders add "Inbox" "Buy milk"
reminders add "Inbox" "Submit timesheet" --due-date "Friday 5pm"
reminders add "Inbox" "Call dentist" --notes "Schedule cleaning" --priority 1
# priority: 1=high, 5=medium, 9=low
```

## Mutate (index from `show`, 0-based)

```bash
reminders complete|uncomplete|delete "Inbox" 0
reminders edit "Inbox" 0 "New title"
reminders edit "Inbox" 0 --due-date "tomorrow 9am"
```

## Lists

```bash
reminders new-list "Project X"
reminders delete-list "Project X"
```
