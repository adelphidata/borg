---
name: reminders
description: "Manage native macOS Reminders via reminders-cli"
requires:
  bins: ["reminders"]
---

# Reminders Skill

Manage native macOS Reminders.app from the shell using `reminders-cli` (`brew install keith/formulae/reminders-cli`). macOS-only. The first run will prompt for Reminders access via TCC; the user must approve in System Settings → Privacy & Security → Reminders.

## List

```bash
# Show all lists
reminders show-lists

# Show all reminders in a list (incomplete only by default)
reminders show "Inbox"

# Include completed
reminders show "Inbox" --include-completed

# All reminders across all lists
reminders show-all
```

## Add

```bash
# Add to a list
reminders add "Inbox" "Buy milk"

# With a due date (natural language)
reminders add "Inbox" "Submit timesheet" --due-date "Friday 5pm"

# With notes / priority (1=high, 5=medium, 9=low)
reminders add "Inbox" "Call dentist" --notes "Schedule cleaning" --priority 1
```

## Complete / Uncomplete / Delete

```bash
# Show with indices
reminders show "Inbox"

# Complete by index (0-based)
reminders complete "Inbox" 0

# Uncomplete
reminders uncomplete "Inbox" 0

# Delete
reminders delete "Inbox" 0
```

## Edit

```bash
# Change title
reminders edit "Inbox" 0 "New title"

# Reschedule
reminders edit "Inbox" 0 --due-date "tomorrow 9am"
```

## Create / Delete Lists

```bash
reminders new-list "Project X"
reminders delete-list "Project X"
```

## JSON Output

```bash
# Most subcommands accept --format json
reminders show "Inbox" --format json
reminders show-lists --format json
```

## Notes

- macOS only. On other platforms, fall back to a different task system (e.g., the `notes` skill or Linear).
- Indices shift after completion/deletion — always re-`show` before referencing one.
- Due dates accept natural language (`"Friday 5pm"`, `"tomorrow"`, `"in 2 hours"`).
- `reminders` CLI is read-only with respect to other Apple devices — changes sync via iCloud automatically.
