# Evolution System

Borg evolves based on how you actually use it. Like Pokémon, evolution is a permanent transformation earned through sustained usage — not a toggle or setting. Your agent develops a unique specialization and name based on your real workflows.

The evolution system has three axes:

1. **Classes (Stages)** — three permanent evolution tiers (Base → Evolved → Final)
2. **Levels** — 0–99 within each stage, providing continuous progression
3. **Archetypes** — 10 internal categories that classify your usage pattern and drive the generated evolution name

Display format: `Pipeline Warden Lvl.42` (LLM-generated name + current level).

---

## Classes (Stages)

Stages are **permanent** — once your agent evolves, it never regresses. The active archetype can drift over time; the evolution **name** granted at the transition is kept in the ledger forever.

| # | Variant | Label | Share-card label |
|---|---------|-------|------------------|
| 1 | `Stage::Base` | Base Borg | `Base I` |
| 2 | `Stage::Evolved` | Evolved Borg | `Evolved II` |
| 3 | `Stage::Final` | Final Borg | `Final III` |

Each class has its own 0–99 level progression. Reaching Lvl.99 is a hard gate for transitioning to the next stage.

Autonomy is **not** derived from stage — it's a separate concept owned by the Bond module (`crates/core/src/bond.rs`, `AutonomyTier` with 5 variants: `ObserveOnly`, `Recommend`, `DraftAssist`, `GuidedAction`, `HighTrust`) driven by bond score. See `docs/bond.md`.

Source: `crates/core/src/evolution/mod.rs` (`Stage` enum), `crates/core/src/evolution/share_card.rs`.

---

## Archetypes

Archetypes classify how you use Borg. The LLM uses your dominant archetype to generate a unique evolution name, and archetype-aligned actions earn bonus XP.

| Archetype | Domain | Fallback description |
|-----------|--------|----------------------|
| **Ops** | Infrastructure, deployment, SRE | "A vigilant DevOps guardian keeping your builds green and deploys smooth." |
| **Builder** | Software building, compilation, automation | "A restless builder who'd rather automate a task once than do it twice." |
| **Analyst** | Data analysis, querying, research | "A patient investigator who turns raw signal into decisions." |
| **Communicator** | Messaging, email, outreach | "A relentless communicator turning cold leads warm and inboxes manageable." |
| **Guardian** | Security, auditing, hardening | "A careful sentinel watching the gates so you don't have to." |
| **Strategist** | Planning, prioritization, decisions | "A calm planner laying out the next move before it's needed." |
| **Creator** | Content, writing, documentation | "A thoughtful writer shaping words and narratives with care." |
| **Caretaker** | Home, wellness, personal rhythms | "A quiet steward keeping the household rhythms on beat." |
| **Merchant** | E-commerce, sales, finance | "A meticulous keeper of ledgers and commerce flows." |
| **Tinkerer** | Homelab, hardware, experimentation | "A curious hacker who can't leave a homelab alone for five minutes." |

Source: `crates/core/src/evolution/mod.rs` (`Archetype` enum), `crates/core/src/evolution/classification.rs`.

### Signal Classification

Actions are classified to archetypes via three layers, in order:

1. **Deterministic tool mapping** — tool names map directly:
   - `apply_patch`, `apply_skill_patch`, `create_channel` → **Builder**
   - `browser`, `search`, `memory_search` → **Analyst**
   - `calendar`, `notion`, `linear`, `schedule`, `manage_tasks` → **Strategist**
   - `gmail`, `telegram`, `slack`, `discord`, `whatsapp`, `sms` → **Communicator**
   - `write_memory` → **Creator**
   - `docker`, `git`, `database` → **Ops**
2. **Keyword matching** — shell command content matched against per-archetype keyword sets (see below).
3. **LLM classification** — ambiguous custom workflows classified on first use, result cached.

### Keyword Sets (shell command classification)

| Archetype | Representative keywords |
|-----------|--------------------------|
| **Ops** | deploy, kubernetes, k8s, docker, terraform, ansible, nginx, systemctl, helm, pipeline, prometheus, grafana, kubectl, argocd, istio, consul, vault, pulumi, cloudformation, cdk, aws, gcloud, azure, lambda, ecs, datadog, pagerduty, rollback, canary, traefik, certbot |
| **Builder** | cargo, npm, pip, gcc, make, build, compile, lint, rustc, webpack, vite, yarn, pnpm, bun, deno, gradle, maven, cmake, bazel, clang, tsc, rollup, turbopack, prettier, eslint, clippy, pytest, jest, vitest, dotnet, xcodebuild |
| **Analyst** | query, select, aggregate, report, analyze, csv, data, metric, psql, mysql, postgres, mongodb, elasticsearch, pandas, jupyter, dataframe, pivot, dashboard, bigquery, redshift, snowflake, dbt, etl, parquet, olap, tableau, powerbi, looker, clickhouse, influxdb, regression, forecast, anomaly |
| **Guardian** | firewall, ufw, iptables, nmap, chmod, chown, audit, vulnerability, cve, openssl, tls, certificate, encrypt, hmac, jwt, oauth, saml, kerberos, selinux, fail2ban, wireshark, tcpdump, pentest, metasploit, owasp, sast, trivy, cosign, gpg, secret, rotation, soc2, gdpr, hipaa |
| **Strategist** | plan, prioritize, compare, evaluate, decision, roadmap, okr, kpi, milestone, sprint, backlog, epic, kanban, scrum, retro, stakeholder, budget, forecast, estimate, timeline, dependency, tradeoff, rfc, adr, objective, quarterly, benchmark |
| **Tinkerer** | homelab, proxmox, pve, esxi, truenas, pihole, wireguard, tailscale, raspberry, arduino, gpio, mqtt, zigbee, esp32, stm32, fpga, oscilloscope, i2c, spi, uart, can-bus, modbus, home-assistant, openwrt, pfsense, opnsense, unifi, vlan, zfs, btrfs, octoprint, klipper, qemu, lxc |

(Full lists in `crates/core/src/evolution/classification.rs`.)

### Archetype Scoring

Each archetype maintains a rolling score:

```
effective_score = lifetime_score * 0.35 + last_30d_score * 0.65
```

Recent behavior steers specialization. A user pivoting from DevOps to marketing will see their dominant archetype shift over weeks.

---

## Evolutions (Generated Names)

When a stage gate passes, Borg calls the LLM to mint a unique name + description based on:

- Dominant archetype (e.g., `ops`)
- Target stage (`Base` / `Evolved` / `Final`)
- Top 5 tools by usage with counts (e.g., `docker (×47), kubectl (×39)`)

The LLM returns strict JSON: `{"name": "<2-4 words, evocative, title case>", "description": "<1-2 sentences>"}`. Timeout: 30 seconds. On failure or timeout, Borg falls back to a hardcoded name per archetype/stage.

### Fallback Evolution Names

| Archetype | Stage 2 (Evolved) | Stage 3 (Final) |
|-----------|-------------------|-----------------|
| Ops | **Pipeline Warden** | **Infrastructure Sovereign** |
| Builder | **Tool Forgemaster** | **Automation Architect** |
| Analyst | **Insight Diviner** | **Pattern Oracle** |
| Communicator | **Outreach Operative** | **Signal Weaver** |
| Guardian | **Vigilant Sentinel** | **Fortress Keeper** |
| Strategist | **Path Finder** | **Grand Planner** |
| Creator | **Word Smith** | **Narrative Architect** |
| Caretaker | **Gentle Steward** | **Household Guardian** |
| Merchant | **Ledger Keeper** | **Commerce Sage** |
| Tinkerer | **Bench Wizard** | **Homelab Artisan** |

Source: `crates/core/src/evolution/classification.rs`.

The evolution name is permanent — it's recorded in the `evolution_events` ledger and stays part of the agent's history even if the dominant archetype later drifts.

---

## Levels (0–99)

Each class has its own level progression with an exponential XP curve.

### XP Formula

```
xp_for_level(stage, n) = stage_base + floor(n ^ stage_curve)
```

| Stage | Base | Curve | Lvl.1 cost | Lvl.50 cost | Lvl.99 cost | Target duration |
|-------|------|-------|------------|-------------|-------------|-----------------|
| 1 (Base) | 20 | 1.4 | 21 XP | 259 XP | 642 XP | 2–5 days |
| 2 (Evolved) | 40 | 1.55 | 41 XP | 469 XP | 1,279 XP | 6–12 months |
| 3 (Final) | 80 | 1.8 | 81 XP | 1,223 XP | 3,989 XP | 1–2+ years |

Hard cap: level 99. Source: `crates/core/src/evolution/xp.rs`.

### XP Awarded per Action

| Action | Base XP | Aligned bonus |
|--------|---------|---------------|
| Creation (`apply_patch`, `apply_skill_patch`, `create_channel`, `write_memory`) | +2 | +1 |
| Successful tool call | +1 | +1 |
| Session interaction (`session_start`) | +1 | — |
| Tool failure | 0 | — |
| Correction detected | 0 | — |

Max per event: **3 XP** (aligned creation).

### Rate Limits (anti-gaming)

Enforced during both write and replay:

| Limit | Value |
|-------|-------|
| Total evolution events / hour | 40 |
| `xp_gain` events / hour | 15 |
| Per-source (per tool) events / hour | 5 |
| `evolution` transitions / hour | 3 |
| `classification` events / hour | 3 |
| `archetype_shift` events / hour | 5 |
| `level_up` events / hour | 10 |
| `milestone_unlocked` events / hour | 3 |
| `mood_changed` events / hour | 5 |
| `share_card_created` events / hour | 3 |
| Default for other event types | 10 |

---

## Evolution Gates

### Stage 1 → Stage 2 (Base → Evolved)

All four hard gates must pass simultaneously:

| Requirement | Threshold |
|-------------|-----------|
| Level | Lvl.99 at Stage 1 |
| Bond score | ≥ 30 |
| Minimum vital | Every vital ≥ 20 |
| Dominant archetype | Top score ≥ 1.3× runner-up (or runner-up = 0) |

### Stage 2 → Stage 3 (Evolved → Final)

All four hard gates must pass simultaneously:

| Requirement | Threshold |
|-------------|-----------|
| Level | Lvl.99 at Stage 2 |
| Bond score | ≥ 55 |
| Correction rate | < 20% over the last 14 days (corrections + negative sentiment ÷ total vitals events) |
| Archetype stability | Dominant archetype unchanged for ≥ 14 days |

### Final Form (Stage 3)

No further evolution. Reaching Lvl.99 in Final Form represents true mastery. The exponential XP curve ensures the last 20 levels feel like a genuine achievement.

Source: `crates/core/src/evolution/mod.rs`.

---

## Milestones

Sub-evolution wins, detected by `check_milestones(prev, next)` in `crates/core/src/evolution/milestones.rs`. Each milestone ID fires **at most once** (deduped against `milestone_unlocked` rows in the ledger).

### Level Milestones

Thresholds: **10, 25, 50, 75, 99**. ID format: `level_{threshold}_{stage}` (e.g., `level_25_evolved`). Triggered when level crosses a threshold upward within the same stage — not across stage transitions.

### Fixed Milestones

| Milestone ID | Title | Trigger |
|--------------|-------|---------|
| `first_evolution` | First Evolution | Stage 1 → Stage 2 transition |
| `first_strong_bond` | Strong Bond | Bond score crosses 55 upward |
| `archetype_stabilized` | Archetype Stabilized | Dominant archetype held ≥ 7 days |
| `aligned_streak_7d` | 7-Day Aligned Streak | 7 consecutive UTC days with ≥ 1 archetype-aligned `xp_gain` |

---

## Mood

A lossy UX signal (not authoritative) surfaced in the TUI ambient header.

| Mood | Rule (evaluated in order) |
|------|---------------------------|
| **Ascending** | Lvl.99 at a non-final stage + bond ≥ 30 |
| **Strained** | Any vital < 30 |
| **Drifting** | No dominant archetype yet |
| **Learning** | Stage::Base + growth ≥ 60 + total_xp > 0 |
| **Focused** | focus ≥ 70 + stability ≥ 60 |
| **Stable** | Default fallback |

Source: `crates/core/src/evolution/helpers.rs`.

---

## Momentum (Trend)

Per-archetype direction indicator shown with ↑/↓/= arrows in `/evolution`.

| Trend | Rule |
|-------|------|
| **Rising** | Recent 7d XP − prior 7d XP > 2 |
| **Falling** | Prior 7d XP − recent 7d XP > 2 |
| **Stable** | Within tolerance |

Windows: last 7 days vs. 7–14 days ago. Archetypes absent from both windows are omitted.

---

## What Evolution Unlocks

### Identity Injection

Evolution context is injected into the system prompt each turn:

```xml
<evolution_context>
Stage: Evolved | Pipeline Warden Lvl.42
Archetype: Ops (score: 74)
</evolution_context>
```

The `Archetype:` line is omitted when there is no dominant archetype yet. An Ops-specialized agent defaults to infrastructure-oriented solutions and proactively flags DevOps concerns.

### Cosmetic

- **Stage 1** — default status bar.
- **Stage 2** — evolution name + level shown in status bar and `/status`.
- **Stage 3** — prestige badge, enhanced `/status` with evolution history timeline.

### Ambient TUI Header

Shows name, level, mood, and dominant archetype (e.g. `Pipeline Warden Lv.42 — Focused — Ops`). Refreshes on `AfterToolCall` via cached `AmbientStatus`. Gated on `evolution.ambient_header_enabled` (default `true`).

---

## Status Surfaces

All status commands share one dispatcher at `borg_core::evolution::{parse, dispatch, CommandOutput, EvolutionCommand}` — TUI and every messaging channel render identical text.

| Command | Content |
|---------|---------|
| `/evolution` | Current stage, level, archetype scores (with momentum arrows), readiness to next stage, 1–3 next-step hints |
| `/xp` | Today's & weekly XP totals, top sources, archetype breakdown, last 10 feed entries (`xp_gain` + `level_up` + `milestone_unlocked`) |
| `/card` | Boxed ASCII share card — name, level, stage, archetype, one-line description |

Channel commands are intercepted by the gateway command registry (`crates/gateway/src/commands.rs`) **before** the message reaches the agent loop — deterministic status reads, no LLM turn invoked, no `messages` row recorded. Works across Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage.

---

## Event Sourcing

Evolution state is **event-sourced**. The `evolution_events` table is the single source of truth — no mutable state table.

### HMAC Chain

Each event carries an HMAC-SHA256 signature chained from the previous event. During replay, events with broken chains are skipped.

```
hmac_content = "{event_type}|{xp_delta}|{archetype}|{source}|{created_at}|{prev_hmac}"
```

### Event Types

| Event type | XP Δ | Description |
|------------|------|-------------|
| `xp_gain` | +N | Base or bonus XP. Metadata: `archetype_aligned`, `bonus_reason`, `tool` |
| `evolution` | 0 | Stage transition marker |
| `classification` | 0 | LLM archetype classification result |
| `archetype_shift` | 0 | Dominant archetype changed |
| `level_up` | 0 | Emitted by replay at level boundary. Metadata: `{from_level, to_level, stage}` |
| `milestone_unlocked` | 0 | Sub-evolution win. Metadata: `{milestone_id, title, archetype?}` |
| `mood_changed` | 0 | Ambient-header mood flipped. Metadata: `{from_mood, to_mood, reason}` |
| `share_card_created` | 0 | `/card` rendered. Metadata: `{card_id, kind}` |

### `evolution_events` Schema (V24)

| Column | Type | Notes |
|--------|------|-------|
| id | INTEGER PRIMARY KEY | Auto-increment |
| event_type | TEXT NOT NULL | See table above |
| xp_delta | INTEGER NOT NULL DEFAULT 0 | 0 for non-XP events |
| archetype | TEXT | Related archetype |
| source | TEXT NOT NULL | Tool name, `session_start`, etc. |
| metadata_json | TEXT | Classification, descriptions, LLM output |
| created_at | INTEGER NOT NULL | Unix timestamp |
| hmac | TEXT NOT NULL | HMAC-SHA256 |
| prev_hmac | TEXT NOT NULL DEFAULT '0' | Chain link |

Indexes on `created_at` and `archetype`.

---

## Celebrations

Stage-transition and milestone celebrations ride the same `pending_celebrations` outbox. Delivery honors `config.heartbeat.channels`.

### Stage-Transition Art

**Base → Evolved:**

```
          .  .
         /(..)\
        ( (")(") )          /\_____/\
         \  ~ /    -->    (  o . o  )
          ~~~~             > ^ ^ ^ <
                           /_______\
```

**Evolved → Final:**

```
       /\_____/\           __/|__
      (  o . o  )   -->   / o.O  \___
       > ^ ^ ^ <         |  __    __ \
       /_______\         | /  \  /  ||
                         |_\__/  \__/|
                          \_________/
```

Title: `* * *  F I N A L   F O R M  * * *` on Final transitions, `* * *  E V O L U T I O N  * * *` otherwise.

### Milestone Format

Simpler `MILESTONE UNLOCKED` card showing title, level, stage, and (if any) archetype. Inner width 36 chars.

Source: `crates/core/src/evolution/celebration.rs`.

---

## Hook Integration

`EvolutionHook` implements the `Hook` trait and wraps `Database` in `Mutex<Database>` (same pattern as `VitalsHook`).

| Hook point | Action |
|------------|--------|
| `SessionStart` | Check evolution gates; trigger evolution if met |
| `BeforeAgentStart` | `InjectContext` with evolution summary |
| `BeforeLlmCall` | `InjectContext` with evolution summary |
| `AfterToolCall` | Record XP event (base + archetype bonus) |

Always returns `Continue` except at `BeforeAgentStart` / `BeforeLlmCall`.

---

## Architecture

| File | Role |
|------|------|
| `crates/core/src/evolution/mod.rs` | Core types (`Stage`, `Archetype`, `Mood`, `Trend`), scoring, gate logic, `EvolutionHook` |
| `crates/core/src/evolution/xp.rs` | XP curves and per-stage level costs |
| `crates/core/src/evolution/classification.rs` | Keyword sets, tool mapping, LLM name generation, fallback names/descriptions |
| `crates/core/src/evolution/milestones.rs` | Milestone detection and dedup |
| `crates/core/src/evolution/celebration.rs` | ASCII art, celebration formatting |
| `crates/core/src/evolution/share_card.rs` | `/card` renderer |
| `crates/core/src/evolution/commands.rs` | `/evolution`, `/xp`, `/card` dispatcher |
| `crates/core/src/evolution/feed.rs` | `/xp` feed entries |
| `crates/core/src/evolution/format.rs` | Shared formatting helpers |
| `crates/core/src/evolution/helpers.rs` | Mood computation, momentum windows |
| `crates/core/src/evolution/hmac.rs` | HMAC chain helpers |
| `crates/core/src/evolution/replay.rs` | Event replay → state |
| `crates/core/src/db.rs` | V24 migration, `evolution_events` CRUD |
| `crates/cli/src/main.rs` | `borg status` integration |
| `crates/cli/src/tui/mod.rs`, `crates/cli/src/repl.rs` | `EvolutionHook` registration |
