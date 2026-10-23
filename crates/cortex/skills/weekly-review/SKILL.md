---
name: weekly-review
description: "Run a weekly review and plan: look back at the past week's calendar, tasks, notes, and flagged mail; find what slipped, stalled, or waits on others; then build a realistic plan for next week. Proposes changes for approval. Works well as a scheduled Friday or Sunday job."
category: productivity
triggers:
  - "run my weekly review"
  - "plan next week"
  - "what did I commit to"
  - "what is slipping"
  - "weekly reset"
---

# Weekly Review

A bounded weekly reset across the systems the user already uses. Read widely;
change nothing until the user approves.

## Sources (use what is connected)

- Calendar: `google_calendar` tool, or the `calendar` skill.
- Tasks: the `reminders`, `linear`, or `notion` skills/tools, or a tasks file.
- Notes: `memory_search` first, then `notes`, `apple-notes`, or `obsidian`.
- Mail: flagged or starred threads only (`gmail` or the `email` skill). Full
  triage belongs to the `inbox-triage` skill.

Check `memory_search` for a saved setup ("weekly review: tasks in Linear,
Google calendar, Europe/Berlin"). If there is none, ask once and save the
answer with `write_memory` so later runs need no questions.

## 1. Set the window

Timezone, review week (default: last Monday to Sunday), planning horizon
(default: next 7 to 14 days), which system wins when two disagree, and what
you may change (default: nothing; proposals only).

## 2. Look back

From last week's calendar: meetings held, follow-ups they imply, promises made.
From tasks: done, overdue, and moved. Record wins too.

## 3. Clear the inboxes

Task inbox, loose notes, flagged mail, other capture points. Give each item one
outcome: next action, project, waiting, scheduled, someday, reference, or
delete (proposed). Count what is still unprocessed.

## 4. Check each active project

Outcome, next action, owner, due date, blocker, last real activity, and link.
Flag projects with no next action, a missed date, duplicates, or conflicting
status. Each project ends actionable or explicitly paused.

## 5. Waiting and promises

List what the user promised others and what others owe the user. Propose a
follow-up date and channel for each. Silence is not completion.

## 6. Plan to capacity

Add up next week's fixed calendar load. Pick 3 to 5 outcomes and their next
actions, ranked by consequence, deadline, dependency, and effort. Leave slack;
do not fill every free hour. Say what you deferred and why.

## 7. Apply approved changes

Update tasks, add calendar holds, archive processed items, and draft
follow-ups, but only what the user approved. Read each change back from its
system.

## Report

1. Wins and finished commitments
2. Overdue or at risk
3. Waiting and follow-ups, with dates
4. Stalled or unclear projects
5. Next week: outcomes, fixed load, open time
6. Proposed changes awaiting approval
7. Gaps: sources not checked or not reachable

## Make it recurring

Offer a `schedule` job, for example Friday 4pm local time:

```json
{"action": "create", "type": "prompt", "name": "Weekly review",
 "prompt": "Run my weekly review with the weekly-review skill. Propose changes; apply none.",
 "schedule_type": "cron", "schedule_expr": "0 16 * * 5"}
```

A scheduled run cannot wait for approval, so it reports and proposes only.
