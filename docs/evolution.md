# Evolution System

Borg evolves based on how you actually use it. Like Pokemon, evolution is a permanent transformation earned through sustained usage — not a toggle or setting. Your agent develops a unique specialization and name based on your real workflows.

## Overview

The evolution system has three components:

1. **Stages** — three permanent evolution tiers (Base → Evolved → Final)
2. **Levels** — 0–99 within each stage, providing continuous progression
3. **Archetypes** — 10 internal categories that classify your usage pattern

Display format: `Pipeline Warden Lvl.42` (LLM-generated name + current level).

## Stages

| Stage | Name | What it means |
|-------|------|---------------|
| 1 | **Base Borg** | Generic agent. No specialization yet. Learning your patterns. |
| 2 | **Evolved Form** | Specialization emerges. Agent receives a unique LLM-generated name tied to an archetype. |
| 3 | **Final Form** | Mastery. Deep specialization confirmed through sustained, consistent usage. |

Stages are **permanent** — once your agent evolves, it never regresses. However, the specialization type (archetype) can drift over time as your usage patterns change.

## Levels (0–99)

Each stage has its own level progression from Lvl.0 to Lvl.99. Reaching Lvl.99 is one of the requirements for evolving to the next stage.

Levels are driven by XP. All actions earn base XP, but actions aligned with your dominant archetype earn bonus XP. This rewards specialization without punishing variety.

### XP Formula

```
xp_gained = base_xp + (archetype_bonus if action aligns with dominant archetype)
```

| Action | Base XP | Archetype Bonus |
|--------|---------|-----------------|
| Successful tool call | +1 | +1 if archetype-aligned |
| Creation event (tool, skill, memory, channel) | +2 | +1 if archetype-aligned |
| Session interaction | +1 | — |
| Scheduled task success | +2 | +1 if archetype-aligned |
| Tool failure | 0 | — |
| Correction detected | 0 | — |

### Level Curve (WoW-Style)

The goal is **slow, earned progression** — each level should feel meaningful. Early levels take about a day, late levels take weeks. Reaching Lvl.99 in Final Form is a multi-year journey.

XP required per level scales **per stage** with increasing exponential curves:

```
xp_for_level(stage, n) = stage_base + floor(n ^ stage_curve)
```

| Stage | Base | Curve | Lvl.1 cost | Lvl.50 cost | Lvl.99 cost | Target duration |
|-------|------|-------|------------|-------------|-------------|-----------------|
| 1 (Base) | 20 | 1.4 | 21 XP | 259 XP | 642 XP | **months** |
| 2 (Evolved) | 40 | 1.55 | 41 XP | 469 XP | 1,279 XP | **6-12 months** |
| 3 (Final) | 80 | 1.8 | 81 XP | 1,223 XP | 3,989 XP | **1-2+ years** |

**Pacing at ~40 XP/day (active user, 2-3 sessions):**

- Level 0: ~0.5 days — Level 10: ~1 day — Level 50: ~6.5 days — Level 90: ~14 days (2 weeks)
- Progression is slow and deliberate — levels are milestones, not participation trophies
- Late-stage levels (90+) take 2+ weeks each

### XP Rate Limiting

To prevent gaming, XP events are rate-limited during replay:

| Limit | Value |
|-------|-------|
| Total XP events per hour | 15 |
| Same source per hour | 5 |
| Creation events per hour | 3 |

Events exceeding limits are silently ignored during state computation.

## Archetypes

Archetypes are internal categories that classify how you use Borg. The LLM uses your archetype to generate a unique evolution name, and archetype-aligned actions earn bonus XP.

| Archetype | Domain | Example signals |
|-----------|--------|-----------------|
| **Ops** | DevOps, SRE, infra, CI/CD | Docker, Kubernetes, deploy scripts, monitoring, shell-heavy workflows |
| **Builder** | Automation, coding, creation | `apply_patch`, `apply_skill_patch`, multi-step automations |
| **Analyst** | Research, data, metrics, reporting | Browser research, data queries, database tools, comparison workflows |
| **Communicator** | Outreach, messaging, email, DMs | Telegram, Slack, Discord, Gmail, LinkedIn, email campaigns |
| **Guardian** | Security, compliance, monitoring | `security_audit`, blocked path checks, host audit, vulnerability scanning |
| **Strategist** | Planning, decision-making, prioritization | Planning sessions, decision queries, weekly calibrations, summary requests |
| **Creator** | Content, writing, marketing, docs | Content generation, blog drafts, documentation, creative writing |
| **Caretaker** | Home, wellness, personal management | Habit tracking, meal planning, family reminders, personal routines |
| **Merchant** | E-commerce, sales, finance | Transaction tools, sales workflows, pricing analysis, CRM integrations |
| **Tinkerer** | Hardware, homelab, experimentation | Self-hosted tools, custom integrations, network config, hardware automation |

### Archetype Scoring

Each archetype maintains a rolling score computed from usage signals:

```
effective_score = lifetime_score * 0.35 + last_30d_score * 0.65
```

This lets current behavior steer specialization. A user who pivots from DevOps to marketing will see their dominant archetype shift over weeks.

### Signal Classification

Actions are classified to archetypes using a combination of:

1. **Deterministic rules** — tool names, skill names, and integration types map directly (e.g., `docker` skill → Ops, `apply_patch` → Builder)
2. **Keyword matching** — shell command content and task prompts are matched against archetype keyword sets
3. **LLM classification** — for ambiguous custom workflows (e.g., a user-created "amazon-cart-optimizer" tool), the LLM classifies on first use and caches the result

#### Deterministic Tool → Archetype Mapping

| Tools / Skills | Archetype |
|---------------|-----------|
| `docker`, `git`, `database`, shell commands with deploy/k8s/terraform keywords | Ops |
| `apply_patch`, `apply_skill_patch`, `create_channel` | Builder |
| `browser` (research), `search`, database queries, `read_pdf` | Analyst |
| `telegram`, `slack`, `discord`, `gmail`, messaging channels | Communicator |
| `security_audit`, `1password`, shell commands with security/firewall keywords | Guardian |
| `calendar`, `notion`, `linear`, planning/summary queries | Strategist |
| `write_memory` (content), notes, long-form generation | Creator |
| Wellness/health/family-related scheduled tasks | Caretaker |
| Sales/commerce/finance-related tools and tasks | Merchant |
| Custom tools, self-hosted integrations, hardware-related shell commands | Tinkerer |

#### Keyword Sets (Shell Command Classification)

```
Ops:        deploy, kubernetes, k8s, docker, terraform, ansible, nginx, systemctl,
            journalctl, helm, ci, cd, pipeline, prometheus, grafana
Builder:    cargo, npm, pip, gcc, make, build, compile, test, lint, patch
Analyst:    query, select, aggregate, report, analyze, csv, json, data, metric
Guardian:   firewall, ufw, iptables, ssh, chmod, chown, audit, vulnerability, cve
Strategist: plan, prioritize, compare, evaluate, decision, roadmap, okr
Tinkerer:   homelab, proxmox, pve, esxi, truenas, pihole, wireguard, tailscale,
            raspberry, arduino, serial, gpio, mqtt
```

## Evolution Gates

### Stage 1 → Stage 2 (Base → Evolved) — Target: 2-5 days

Designed for quick payoff. The user should feel their agent "come alive" within the first week.

| Requirement | Threshold | Weight |
|-------------|-----------|--------|
| Level | Lvl.99 at Stage 1 | Hard gate |
| Bond score | ≥ 30 | Hard gate (low bar — early trust) |
| Dominant archetype | Top archetype score ≥ 1.3x second-place | Hard gate |
| Vitals health | No vitals stat below 20 | Hard gate |

When all gates pass, the system triggers an **LLM classification call**:
- Input: tool usage distribution, top archetype scores, installed integrations, recent task prompts
- Output: archetype confirmation + unique evolution name + cute description
- The name becomes the agent's Stage 2 identity

### Stage 2 → Stage 3 (Evolved → Final) — Target: ~30 days

Requires real commitment and consistency.

| Requirement | Threshold | Weight |
|-------------|-----------|--------|
| Level | Lvl.99 at Stage 2 | Hard gate |
| Bond score | ≥ 55 | Hard gate |
| Correction rate | < 20% over last 14 days | Hard gate |
| Archetype consistency | Dominant archetype unchanged for 14+ days | Hard gate |

Another LLM call generates the Final Form name and description (e.g., "Pipeline Warden" → "Infrastructure Sovereign").

### Lvl.99 at Stage 3 — Target: 6-12 months

No further evolution — this is the endgame. Reaching Lvl.99 in Final Form represents true mastery and sustained long-term usage. The exponential XP curve ensures the last 20 levels feel like a genuine achievement.

## Specialization Drift

The archetype is **fluid** even after evolution. If a "Pipeline Warden" (Ops) starts doing mostly content creation, the archetype scores will shift. Over time, the agent's context injection updates to reflect the new dominant archetype.

The evolution **name** from Stage 2/3 is permanent — it's part of the agent's history. But the active archetype and its bonuses can change.

## What Evolution Unlocks

### Identity (System Prompt)

Evolution context is injected into the system prompt each turn:

```xml
<evolution_context>
Stage: Evolved | Pipeline Warden Lvl.42
Archetype: Ops (score: 74)
Autonomy: DraftAssist
</evolution_context>
```

The agent naturally adjusts its behavior based on its specialization — an Ops-specialized agent will default to infrastructure-oriented solutions, suggest relevant tools, and proactively flag DevOps concerns.

### Autonomy Tiers

| Stage | Tier | Behavior |
|-------|------|----------|
| 1 (Base) | **Observe** | Suggests only. Conservative. Asks before acting. |
| 2 (Evolved) | **Assist** | Drafts before approval. Moderate proactivity. Chains low-risk actions. |
| 3 (Final) | **Autonomous** | Executes routine workflows independently. Strong proactive recommendations. |

Autonomy tiers are informational — they shape the agent's behavior via prompt context but never bypass HITL safety settings.

Higher levels within a stage don't change the autonomy tier, but they do increase archetype bonus XP rates.

### Action Limits by Stage

| Limit | Stage 1 | Stage 2 | Stage 3 |
|-------|---------|---------|---------|
| Tool calls (warn/block) | 50/100 | 75/150 | 100/200 |
| Shell commands (warn/block) | 20/50 | 30/75 | 50/100 |
| File writes (warn/block) | 15/30 | 25/50 | 40/80 |

These override the defaults in `[security.action_limits]` when evolution stage is higher.

### Cosmetic (TUI)

- **Stage 1**: Default status bar
- **Stage 2**: Evolution name + level shown in status bar. Stage badge in `/status` output.
- **Stage 3**: Prestige badge. Enhanced `/status` display with evolution history timeline.

## Status Surfaces

All status commands share a single dispatcher at `borg_core::evolution::{parse, dispatch, CommandOutput, EvolutionCommand}` so the TUI and every messaging channel render identical text.

### `/evolution`

Overview of current stage, level, archetype scores (with momentum arrows), readiness to the next stage, and 1–3 next-step hints. In the TUI opens the Evolution tab of the status popup; on channels replies inline.

- **Momentum arrows** — `↑ ↓ =` next to each archetype score, derived from `compute_momentum` comparing the last 7d of archetype-aligned XP against the prior 7d.
- **Readiness block** — shown when the agent is within reach of the next stage gate. Each blocking gate renders as a progress bar with a one-line hint (e.g. "Bond 48 / 55 — spend more grounded time together").
- **Next-step hints** — short, concrete ("Use 3 more Ops-aligned actions this week").

### `/xp`

Today's and this week's XP totals, the top XP sources, the archetype breakdown, and the last 10 feed entries (unified `xp_gain` + `level_up` + `milestone_unlocked`). TUI opens the Xp tab; channels reply inline.

### `/card`

Boxed ASCII share card — name, level, stage, archetype, one-line description. CLI: `borg status card [--out <path>]`. TUI: `/card` pushes the card into the transcript. Channels: inline ASCII only; `--out` is rejected (channels cannot write to the user's filesystem). PNG rendering is deferred to a future iteration — `CommandOutput.image_png` exists in the signature but is unused today.

### Ambient TUI header

The TUI header's `class:` line becomes an ambient broadcast: `Pipeline Warden Lv.42 — Focused — Ops — Evolution pressure rising`. Refreshes on `AfterToolCall` via a cached `AmbientStatus` — no per-render DB hit.

Gated on `evolution.ambient_header_enabled` (default `true`). Toggle via `/settings` in the TUI or `borg settings set evolution.ambient_header_enabled false`.

### Channel commands

`/evolution`, `/xp`, `/card` are intercepted by the gateway command registry (`crates/gateway/src/commands.rs`) in `resolve_session_and_commands` **before** the message reaches the agent loop. They are deterministic status reads, not agent decisions — no LLM turn is invoked and no `messages` row is recorded. Works across Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage.

## Event Sourcing

Like vitals and bond, evolution state is **event-sourced**. The `evolution_events` table is the single source of truth.

### HMAC Chain

Each event carries an HMAC-SHA256 signature chained from the previous event. During replay, events with broken chains are skipped. This prevents casual SQL tampering.

```
hmac_content = "{event_type}|{xp_delta}|{archetype}|{source}|{created_at}|{prev_hmac}"
```

### State Computation

Current stage, level, and archetype scores are always computed by replaying all verified events from baseline (Stage 1, Lvl.0, all archetype scores at 0). There is no mutable state table — the event ledger is the single source of truth, same as vitals and bond.

### Event Types

| Event Type | XP Delta | Description |
|------------|----------|-------------|
| `xp_gain` | +N | Base or bonus XP from an action. Metadata: `archetype_aligned`, `bonus_reason`, `tool` |
| `evolution` | 0 | Stage transition marker |
| `classification` | 0 | LLM archetype classification result |
| `archetype_shift` | 0 | Dominant archetype changed |
| `level_up` | 0 | Emitted by replay when a level boundary is crossed. `metadata_json: {from_level, to_level, stage}` |
| `milestone_unlocked` | 0 | Sub-evolution win. `metadata_json: {milestone_id, title, archetype?}` |
| `mood_changed` | 0 | Ambient-header mood flipped. `metadata_json: {from_mood, to_mood, reason}` |
| `share_card_created` | 0 | `/card` rendered. `metadata_json: {card_id, kind}` |

### Milestone Triggers

Detected by `check_milestones(prev, next)` in `crates/core/src/evolution/milestones.rs`:

| Milestone ID | Trigger |
|---|---|
| `level_10`, `level_25`, `level_50`, `level_75`, `level_99` | Level boundary crossed within the current stage |
| `first_evolution` | Stage 1 → Stage 2 transition |
| `archetype_stabilized` | Dominant archetype held for 7+ days |
| `first_strong_bond` | Bond crosses 55 for the first time |
| `aligned_streak_7d` | 7 consecutive days of archetype-aligned XP |

Milestones ride the same `pending_celebrations` outbox as stage-transition celebrations. `deliver_celebration_to_channels` branches on `celebration_type` (`"evolution"` vs `"milestone"`) and formats via `format_celebration(CelebrationKind::{Evolution, Milestone})`. Delivery honors `config.heartbeat.channels` — no new configuration surface.

## Database Tables (V24)

### `evolution_events`

Append-only ledger with HMAC chain. This is the **only** evolution table — no mutable state table exists.

| Column | Type | Notes |
|--------|------|-------|
| id | INTEGER PRIMARY KEY | Auto-increment |
| event_type | TEXT NOT NULL | xp_gain, evolution, classification, archetype_shift, level_up, milestone_unlocked, mood_changed, share_card_created |
| xp_delta | INTEGER NOT NULL DEFAULT 0 | XP gained (0 for non-XP events) |
| archetype | TEXT | Which archetype this event relates to |
| source | TEXT NOT NULL | Tool name, "session_start", etc. |
| metadata_json | TEXT | Classification results, evolution details, LLM-generated descriptions |
| created_at | INTEGER NOT NULL | Unix timestamp |
| hmac | TEXT NOT NULL | HMAC-SHA256 of event data |
| prev_hmac | TEXT NOT NULL DEFAULT '0' | Chain link |

Index on `created_at` and `archetype`.

Evolution names, descriptions, and classification results are stored as `evolution` and `classification` event types in `metadata_json`. No separate tables needed — the ledger contains everything.

## Evolution Descriptions

Each evolution gets an LLM-generated description — concise, light, and relevant to the archetype and usage pattern. Descriptions add personality without over-explaining.

Examples:

```
Pipeline Warden Lvl.93
  "A vigilant DevOps guardian that keeps your builds green,
   your deploys smooth, and your chaos politely contained."

Outreach Operative Lvl.34
  "A relentless communicator that turns cold leads warm
   and keeps your inbox at inbox zero (almost)."

Tool Forgemaster Lvl.71
  "A restless builder who'd rather automate a task once
   than do it twice. Your automation stack is its playground."
```

Descriptions are generated by the LLM at evolution time and stored in `metadata_json` of the `evolution` event.

## CLI / TUI

Evolution info is integrated into the existing `borg status` command — no separate `borg evolve` command. The status display combines vitals, bond, and evolution in one view.

### `borg status` (enhanced)

```
Borg Status
───────────

  Pipeline Warden Lvl.42
  "A vigilant DevOps guardian that keeps your builds green,
   your deploys smooth, and your chaos politely contained."

  Stage        ██████████████████░░░░░░░░░░░░░░  Evolved (2/3)
  XP           1,240 / 1,520 to Lvl.43

Vitals
  stability    ████████░░  80
  focus        ██████░░░░  60
  sync         █████░░░░░  55
  growth       ███████░░░  70
  happiness    █████░░░░░  50

Bond
  score        ██████░░░░  68  (Trusted)
```

At Stage 1 (pre-evolution), the header shows:

```
  Base Borg Lvl.14
  Discovering your patterns...
```

Next evolution requirements are hinted at subtly — keeping some mystery around what triggers the next stage.

### `borg status` tabs

The status command supports tab navigation between views:

| Tab | Content |
|-----|---------|
| **Overview** (default) | Evolution name/level/description, stage bar, XP, vitals, bond |
| **Evolution History** | Timeline of stage transitions with dates, names, and archetypes |
| **Archetype Scores** | All 10 archetype scores with bars, dominant archetype highlighted |

In CLI mode, tabs are accessed via `borg status`, `borg status history`, `borg status archetypes`. In TUI mode, `/status` opens an interactive popup with tab switching.

## Celebration Messages

When a stage transition occurs, Borg sends a celebratory ASCII art message to all configured heartbeat channels (Slack, Telegram, etc.) via a database outbox pattern. The ASCII art is configurable per stage and archetype — see `celebration_art()` in `crates/core/src/evolution/mod.rs`.

## Hook Integration

`EvolutionHook` implements the `Hook` trait:

| Hook Point | Action |
|------------|--------|
| `SessionStart` | Check evolution gates; trigger evolution if met |
| `BeforeAgentStart` | `InjectContext` with evolution summary |
| `BeforeLlmCall` | `InjectContext` with evolution summary |
| `AfterToolCall` | Record XP event (base + archetype bonus) |

The hook wraps `Database` in `Mutex<Database>` (same pattern as VitalsHook). Always returns `Continue` except at `BeforeAgentStart`/`BeforeLlmCall` where it returns `InjectContext`.

## Architecture

All evolution logic lives in one file:

| File | Role |
|------|------|
| `crates/core/src/evolution.rs` | Types, scoring, XP curve, archetype classification, HMAC chain, replay, formatting, EvolutionHook |
| `crates/core/src/db.rs` | V24 migration, CRUD methods for evolution_events table |
| `crates/cli/src/main.rs` | Enhanced `borg status` with evolution display |
| `crates/cli/src/tui/mod.rs` | Hook registration, evolution display in session header |
| `crates/cli/src/repl.rs` | Hook registration |
| `crates/core/src/lib.rs` | `pub mod evolution;` |
