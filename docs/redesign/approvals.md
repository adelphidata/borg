# Approvals & Budgets — v1

Status: design, in active rework. Companion to [`auth-and-onboarding.md`](auth-and-onboarding.md), [`architecture-redesign.md`](architecture-redesign.md), [`llm-providers.md`](llm-providers.md).

## Goal

Define the v1 approval surface and budget enforcement. v1 has exactly **one mode**, and it preserves the existing CLAUDE.md invariant verbatim: **the agent just executes.** No per-tool-call approval prompts. Catastrophic commands are denied at the sandbox layer. Budget is the only thing that can stop a turn mid-flight.

The Sentry-mode tiered approval pipeline (Tier 3 prompts via paired device, pre-approval policies, M-of-N quorum, approval coalescence, race resolution, signed policy rows) is a v3 addition and scoped at the bottom of this doc.

## Non-goals

- **Approval prompts in v1.** Explicitly forbidden. If you find yourself adding one, you've misread the design.
- **Hybrid "ask first time, remember after" UX.** Adds prompt fatigue for the rare-but-important cases without buying meaningful safety on top of the sandbox.
- **Live LLM-call cancellation when budget hits.** Mid-stream cancellation wastes the spent tokens and produces broken state. Finish current call, refuse next.

## Locked decisions (v1)

| # | Decision |
|---|---|
| 1 | Zero per-tool-call approval prompts. The agent just executes. |
| 2 | Catastrophic-command denylist (rm -rf /, mkfs, dd, curl\|sh) enforced at sandbox layer. |
| 3 | Tier system exists for **audit categorization only**; no tier requires an interactive prompt in v1. |
| 4 | Tier 3 actions emit a high-priority audit event and surface in `/status` retroactively. They never block. |
| 5 | Budgets: per-principal optional, daemon-wide default required. Min-wins. (No per-workspace budgets — v1 has no workspaces.) |
| 6 | Budget unit: tokens stored, USD displayed via per-provider rate card. |
| 7 | Budget reset: daily default, configurable. Reset is a `budget_reset` event in the chain. |
| 8 | Hard-stop behavior: finish current LLM call, refuse the next. |
| 9 | Default budget: soft-cap $5/day, hard-stop $20/day, configurable via `/settings → Budgets`. |

---

## v1 — the agent just executes

The CLAUDE.md invariant is preserved verbatim:

> **DO NOT add per-tool-call approval prompts or confirmation dialogs.** The agent just executes.
>
> - Shell commands auto-execute. Only catastrophic commands (rm -rf /, mkfs, dd, curl|sh) are denied.
> - No HITL confirmation for tool calls.
> - Sandboxing and rate limiting are security boundaries, not approval dialogs.

What the user *does* see:

| Trigger | UX |
|---|---|
| Catastrophic command attempt | Toast: *"Refused: `rm -rf /`. Catastrophic command denylist."* No prompt to override. |
| Budget at 80% | One System cell: *"Budget at 80% — $4 of $5 today."* |
| Budget at 100% | One System cell: *"Budget hit — agent paused. Raise cap in /settings → Budgets, or wait for daily reset."* Agent finishes current LLM call, refuses next. |
| Posture downgrade to Reduced | Toast from `security-posture.md`: *"Posture: Default → Reduced. Future XP earns at ×0.85."* |
| Tier 3 invocation | One-line System cell: *"Tier 3: agent wrote outside `$BORG_HOME` (`/etc/hosts`). Logged."* No prompt. |

That's the entire user-visible approval surface in v1.

### Tier system in v1

The tier system *exists* — it categorizes actions in the audit log and lets the posture model spot when a workspace is doing more risky things than baseline. **No tier requires an interactive prompt.**

| Tier | Examples | v1 behavior |
|---|---|---|
| **Tier 0 — read** | `read_file`, `list_dir`, `web_fetch`, `memory_search` | Auto |
| **Tier 1 — write-local** | `apply_patch`, `write_memory` | Auto + audit |
| **Tier 2 — execute** | `run_shell` in workspace sandbox | Auto (catastrophic denylist + sandbox only) |
| **Tier 3 — privileged** | Host fs outside `BORG_HOME`, network egress changes, credential-store mutation | Auto + high-priority audit event; surfaces in `/status` retroactively, never blocks |

This sounds permissive. It is. The trust model is: single user, trusted device, sandbox is the boundary. If that's wrong for your use case, you want v3 (Sentry mode).

---

## Budgets

### Schema

```sql
CREATE TABLE budgets (
  id              TEXT PRIMARY KEY,
  scope           TEXT NOT NULL,                  -- "principal:<id>" | "default"
  reset_period    TEXT NOT NULL,                  -- "daily" | "weekly" | "monthly"
  hard_cap_usd    TEXT NOT NULL,                  -- decimal string
  soft_cap_usd    TEXT,                           -- NULL = no soft warning
  current_spent_usd TEXT NOT NULL DEFAULT '0',
  current_period_start INTEGER NOT NULL,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE INDEX idx_budgets_scope ON budgets(scope);
```

(v3 adds `scope = "workspace:<id>"` rows when workspaces exist.)

Effective budget for a call = `min(matching_principal_budget, default_budget)`. Whichever has the lowest remaining headroom wins.

### v1 defaults

| Scope | Soft cap | Hard cap | Reset |
|---|---|---|---|
| `default` | $5 | $20 | daily |
| Per-principal | None | None | N/A |

User edits via `/settings → Budgets`. Soft cap fires one System cell. Hard cap finishes the current LLM call and pauses.

### Budget enforcement pipeline

Each LLM call:

1. **Pre-flight token estimate** = `provider.count_tokens(messages, model)` from `llm-providers.md`.
2. **Pre-flight USD estimate** = tokens × rate card. Sum of (current_spent + estimate).
3. **Soft check**: if pre-flight crosses 80% of soft_cap, emit one toast for the period (debounced).
4. **Hard check**: if pre-flight crosses 100% of hard_cap, refuse the call. Emit `budget_hit` event. Agent reports to user.
5. **Post-flight reconciliation**: actual provider-reported tokens may differ; update `current_spent_usd` with actual. If actual exceeds hard_cap (rare), the *next* call fails — never cancel mid-stream.

`budget_hit` event in the chain includes scope, period_start, hard_cap_usd, attempted_call_estimate. Replayable for audit.

### Budget reset

Daemon's hourly tick checks `current_period_start + reset_period < now`; if so, write `budget_reset` event, set `current_spent_usd = 0`, advance `current_period_start`. The reset is a chain event so audit shows "spent $4.92 in period ending 2026-04-25T00:00:00Z."

### Visibility

`/status` row:

```
Budgets · $0.84 / $5 daily · pace ~$3.10/day
```

No `/budget` command. Reuses the existing `/status` discipline.

---

## Hooks integration

`ApprovalHook` extends the existing hook system (`crates/cortex/src/hooks.rs`):

| Hook point | v1 |
|---|---|
| `PreToolUse` | Pass-through (catastrophic denylist + sandbox only) |
| `PostToolUse` | Audit + budget update |
| `BeforeLlmCall` | Budget pre-flight |
| `AfterLlmCall` | Budget reconcile |
| `SessionStart` | No-op |

(v3 adds tier classification → policy match → approval gate at `PreToolUse`, materializes approver list at `SessionStart`.)

---

## Test posture

- **No-prompt invariant.** Run a Tier 3 action (e.g., write outside `BORG_HOME`); verify the action either proceeds (if sandbox allows) or fails at sandbox layer — never via a prompt.
- **Catastrophic denylist.** `rm -rf /`, `mkfs`, `dd if=/dev/zero of=/dev/sda`, `curl ... | sh` all refused at sandbox layer; refusal does not produce an approval prompt.
- **Tier 3 audit visibility.** A Tier 3 action surfaces a one-line System cell + a high-priority audit event; never blocks the agent.
- **Budget pre-flight refusal.** Daemon at $19.50/$20 daily; pre-flight estimate $0.80 → call refused with `budget_hit`; current spend unchanged.
- **Budget post-flight overshoot.** Daemon at $19.80/$20; estimate $0.10, actual $0.30 → call completes (mid-stream cancellation forbidden), `current_spent = 20.10`, *next* call refused.
- **Budget reset.** Daemon tick after period crosses → `budget_reset` event, `current_spent = 0`, audit chain records.
- **Min-wins composition.** Principal cap $1 daily, default cap $20 daily: principal cap fires first.
- **Provider rate card determinism.** Replayed budget computation from chain produces identical USD totals across runs.

---

## Deferred to v3 (Sentry mode)

When v3 lands, this doc gains a Sentry section that adds:

- **Tier 3 mandatory approval prompts** routed to a paired device with a 30-second fail-closed timeout.
- **Pre-approval policies** — `pre_approval_policies` table, signed by admin cert, version-controlled with the workspace, optional quorum signers.
- **Approval queue model** — parallel prompts, identical-coalesce on `(tool, normalized_args)`, FIFO display.
- **Race resolution** — first-write-wins, loser's decision recorded as `superseded`.
- **Push delivery** — APNs/FCM with flag-only payload (mobile is v2 anyway, so this lands when both v2 and v3 are ready).
- **Per-workspace budgets** — `scope = "workspace:<id>"` rows; min-wins extended.
- **Sentry monthly default reset** — instead of daily.

The v1 schema (`budgets`, hook pipeline, audit categorization) is forward-compatible — v3 additions are new tables and new hook behaviors, not migrations of v1 data.

---

## Open questions (v1)

1. **Soft-cap toast debouncing.** Once-per-period vs once-per-session if a session crosses a period boundary. Lean once-per-period.
2. **Budget rollover.** Unspent budget at period end carries over? Lean no — defeats the cap. Configurable as a power-user opt-in.

---

## References

- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — Tier system, principals, scope
- Companion: [`architecture-redesign.md`](architecture-redesign.md) — Layer 4c tier categorization
- Companion: [`llm-providers.md`](llm-providers.md) — token accounting, rate cards
- Companion: [`threat-model.md`](threat-model.md) — A5 boundary in v1
- Existing approval invariant: `CLAUDE.md` "UX Philosophy — No Approval Prompts"
- Existing hook system: `crates/cortex/src/hooks.rs`
- Existing rate-limit table: `crates/cortex/src/evolution/mod.rs`
