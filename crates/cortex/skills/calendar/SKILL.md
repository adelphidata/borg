---
name: calendar
description: "Check schedule and events via icalBuddy (macOS) or gcalcli (Google Calendar)"
category: core
triggers:
  - "check my calendar"
  - "show upcoming events"
  - "what's on my schedule"
  - "calendar agenda"
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

# Calendar

Use **icalBuddy** for macOS local calendars, **gcalcli** for Google Calendar.
Date ranges use `YYYY-MM-DD`.

## icalBuddy (read-only, macOS)

```bash
icalBuddy eventsToday
icalBuddy eventsToday+7                              # next 7 days
icalBuddy eventsFrom:2026-03-14 to:2026-03-21
icalBuddy -ic "Work" eventsToday+7                   # include calendar
icalBuddy -ec "Birthdays,Holidays" eventsToday       # exclude
icalBuddy -ea eventsToday+30 | grep -i "standup"     # search titles
icalBuddy -nrd -nc -b "" -ps "/ | /" eventsToday     # compact one-line
icalBuddy undoneReminders
```

Calendar names are case-sensitive. `-b ""` strips bullets for clean parsing.

## gcalcli (read + write, Google)

First run: `gcalcli init` for OAuth.

```bash
gcalcli agenda                                       # today
gcalcli agenda "today" "3 days from now"
gcalcli agenda "2026-03-14" "2026-03-21" --details all
gcalcli calw | calm                                  # week / month view
gcalcli search "standup" [--start 2026-03-01]
gcalcli list                                         # calendars
gcalcli --calendar "Work" agenda

gcalcli add --title "Team Sync" --when "tomorrow 2pm" --duration 30 --where "Zoom"
gcalcli quick "Lunch with Alex Friday 12pm"          # natural language
gcalcli delete "Team Sync"                           # interactive by title
```
