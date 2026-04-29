# Vitals System

Borg tracks five health stats that update passively as you use the agent. No maintenance commands needed — just use borg normally and your vitals stay healthy.

## The 5 Vitals

| Vital | What it measures | Range |
|-------|-----------------|-------|
| **stability** | Agent reliability — rises with successful operations, drops with failures and corrections | 0–100 |
| **focus** | Alignment with your intent — rises with clear, successful interactions | 0–100 |
| **sync** | How in-touch you and the agent are — rises with regular use, decays fastest with inactivity | 0–100 |
| **growth** | Capability expansion — rises when you create tools, write memories, or install skills | 0–100 |
| **happiness** | Momentum and energy — rises with any meaningful usage, decays with inactivity | 0–100 |

## Checking Vitals

**CLI:**
```sh
borg status
```

**TUI:**
```
/status
```

Both show a full report with stat bars, recent activity summary, drift warnings, and tips.

On session start, a compact vitals summary is shown automatically in the TUI header.

## How Stats Change

Events are classified into broad categories. New tools and integrations automatically get tracked without configuration:

| Category | What triggers it | Effect |
|----------|-----------------|--------|
| **Interaction** | Session start, any user message | sync +2, focus +1, happiness +1 |
| **Success** | Any tool completes without error | stability +1, focus +1, happiness +1 |
| **Failure** | Any tool errors | stability -2, focus -1, growth -1 |
| **Correction** | User corrects the agent ("that's wrong", "try again", etc.) | stability -3, focus -2, sync -1, happiness -1 |
| **Creation** | write_memory, apply_patch, apply_skill_patch, create_channel | stability +2, focus +1, sync +1, growth +3, happiness +3 |
| **Negative Sentiment** | Strong profanity or hostile frustration from the user | stability -2, focus -1, happiness -10 |

## Inactivity Decay

When you don't use borg, stats decay over time:

| After | Stats affected |
|-------|---------------|
| 24 hours | sync -6, happiness -4 |
| 72 hours | additionally stability -8, focus -8 |
| 7 days | additionally growth -5, stability -5 |

## Drift Warnings

When stats drop below thresholds, drift warnings appear:

- **InactiveTooLong** — no interaction for 48+ hours
- **LowStability** — stability below 30
- **LowSync** — sync below 40
- **LowHappiness** — happiness below 30
- **RepeatedFailures** — 3+ recent tool failures

## Technical Details

- **Event-sourced**: There is no mutable state table. Vitals are computed by replaying all verified events from baseline. This prevents gaming via direct SQL manipulation.
- **HMAC chain**: Each event carries an HMAC-SHA256 signature chained from the previous event. Tampered or injected events are detected and skipped during replay.
- **Rate limiting**: Event categories are capped per hour at write time and replay time. Current caps are interaction 5/hr, success 8/hr, failure 5/hr, correction 3/hr, creation 3/hr, and negative_sentiment 3/hr.
- **Diminishing returns**: Repeating the same source within the same hour yields reduced impact, so spammed identical events taper off even before category caps are hit.
- Events are stored in an append-only ledger (`vitals_events` table in SQLite)
- Tracking happens via a lifecycle hook (`VitalsHook`) registered on the agent
- Decay is lazy — computed when vitals are read, not on a timer
- The hook is purely observational — never modifies agent behavior
