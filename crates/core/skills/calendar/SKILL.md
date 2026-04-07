---
name: calendar
description: "Check schedule and events via icalBuddy (macOS) or gcalcli (Google Calendar)"
category: core
requires:
  any_bins: ["icalBuddy", "gcalcli"]
install:
  icalBuddy:
    brew: ical-buddy
  gcalcli:
    brew: gcalcli
    apt: gcalcli
    url: "https://github.com/insanum/gcalcli"
---

# Calendar Skill

Query calendar events and reminders. Supports **icalBuddy** (macOS local calendars) and **gcalcli** (Google Calendar, cross-platform).

## icalBuddy (macOS)

### Today's Events

```bash
icalBuddy eventsToday
icalBuddy -f eventsToday                          # formatted output
icalBuddy -nc eventsToday                          # no calendar name prefix
```

### Upcoming Events

```bash
icalBuddy eventsToday+3                            # next 3 days
icalBuddy eventsFrom:2026-03-14 to:2026-03-21      # date range
icalBuddy -n eventsToday+7                         # next 7 days, no relative dates
```

### Filtering

```bash
icalBuddy -ic "Work" eventsToday+7                 # specific calendar only
icalBuddy -ec "Birthdays,Holidays" eventsToday     # exclude calendars
icalBuddy -ea eventsToday+30 | grep -i "standup"   # search event titles
```

### Custom Formatting

```bash
icalBuddy -nrd -nc -b "" -ps "/ | /" eventsToday   # compact one-line
icalBuddy -npn -nc -iep "datetime,title" eventsToday # time and title only
```

### Reminders

```bash
icalBuddy undoneReminders
icalBuddy -ic "Tasks" undoneReminders
```

## gcalcli (Google Calendar)

### Setup

```bash
gcalcli init   # authenticate with Google on first run
```

### Today's Events

```bash
gcalcli agenda                                     # today's agenda
gcalcli agenda "today" "tomorrow"                  # explicit today
gcalcli agenda --details all                       # full event details
```

### Upcoming Events

```bash
gcalcli agenda "today" "3 days from now"           # next 3 days
gcalcli agenda "2026-03-14" "2026-03-21"           # date range
gcalcli calw                                       # calendar week view
gcalcli calm                                       # calendar month view
```

### Search Events

```bash
gcalcli search "standup"                           # search by title
gcalcli search "standup" --start "2026-03-01"      # with date filter
```

### Create Events

```bash
gcalcli add --title "Team Sync" --when "tomorrow 2pm" --duration 30 --where "Zoom"
gcalcli quick "Lunch with Alex Friday 12pm"        # natural language
```

### Delete Events

```bash
gcalcli delete "Team Sync"                         # interactive delete by title
```

### Filtering by Calendar

```bash
gcalcli --calendar "Work" agenda                   # specific calendar
gcalcli list                                       # list all calendars
```

## Notes

- **icalBuddy**: Install via `brew install ical-buddy`. Calendar names are case-sensitive. First run may prompt for calendar access.
- **gcalcli**: Install via `brew install gcalcli` or `pip install gcalcli`. Requires Google OAuth on first run.
- Use `-b ""` with icalBuddy to remove bullet prefixes for cleaner parsing.
- Dates use `YYYY-MM-DD` format in range queries.
