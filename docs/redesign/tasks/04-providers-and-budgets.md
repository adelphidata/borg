# Task 04 — LLM provider plugin layer + budgets

> Read first: [`llm-providers.md`](../llm-providers.md), [`approvals.md`](../approvals.md).
> Depends on: Task 01 (daemon owns LLM calls).

## Goal

Replace the current ad-hoc per-provider call sites with a uniform `Provider` trait, a versioned rate-card table, and pre/post-flight budget enforcement. Three provider categories in v1: `PublicCloud` (OpenAI, Anthropic, Gemini, Groq, DeepSeek, OpenRouter), `Local` (Ollama, Claude Code CLI), `BYOEndpoint` (any OpenAI-compatible URL the user pastes in).

## Scope

### `Provider` trait

```rust
trait Provider: Send + Sync {
    fn id(&self) -> &str;
    fn category(&self) -> ProviderCategory;
    fn count_tokens(&self, msgs: &[Message], model: &str) -> Result<TokenCount>;
    async fn call(&self, req: ProviderRequest) -> Result<ProviderStream>;
    fn supports_tools(&self) -> bool;
    fn supports_streaming(&self) -> bool;
}
```

`ProviderStream` yields `ProviderEvent`s (tokens, tool calls, completion metadata). `ProviderCallMetadata` (final event) carries actual prompt/completion tokens reported by the provider — single source of truth for budget post-flight.

### Schemas

```sql
CREATE TABLE providers (
  id TEXT PRIMARY KEY,
  category TEXT NOT NULL,            -- PublicCloud | Local | BYOEndpoint
  config_json TEXT NOT NULL,         -- endpoint, headers, etc.
  enabled INTEGER NOT NULL DEFAULT 1,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);

CREATE TABLE provider_rate_cards (
  provider_id TEXT NOT NULL,
  model TEXT NOT NULL,
  effective_at INTEGER NOT NULL,     -- unix ts; lookup uses MAX(effective_at) <= call_ts
  prompt_per_mtok_usd TEXT NOT NULL, -- decimal string
  completion_per_mtok_usd TEXT NOT NULL,
  cached_per_mtok_usd TEXT,          -- NULL if no cache pricing
  PRIMARY KEY (provider_id, model, effective_at)
);
```

Rate card lookup is by `(provider, model, call_ts)`. Versioning by `effective_at` keeps replay deterministic — a budget computation from the chain at audit time uses the same rate row that was active at call time.

### Refresh

- Bundled rate card seeded at install/upgrade.
- Opt-in command `borg providers refresh-rates` pulls latest published rates from a manifest URL (signed, served from the Borg homepage). Default off — no surprise outbound traffic.

### Budgets (`approvals.md`)

```sql
CREATE TABLE budgets (
  id TEXT PRIMARY KEY,
  scope TEXT NOT NULL,                -- "principal:<id>" | "default"
  reset_period TEXT NOT NULL,         -- daily | weekly | monthly
  hard_cap_usd TEXT NOT NULL,
  soft_cap_usd TEXT,
  current_spent_usd TEXT NOT NULL DEFAULT '0',
  current_period_start INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX idx_budgets_scope ON budgets(scope);
```

Defaults: `default` scope, soft $5, hard $20, daily reset. No per-principal budget out of the box.

### Pipeline (per LLM call)

1. Pre-flight: `count_tokens` → estimate USD via rate card → `min(matching principal cap, default cap)` → soft toast at 80% (debounced once per period) → refuse if hard cap crossed (`budget_hit` audit event).
2. Make call.
3. Post-flight: take `ProviderCallMetadata.actual_tokens` → recompute USD → write to `current_spent_usd`. If actual > hard cap (rare overshoot), the *next* call refuses. Never cancel mid-stream.

### Reset tick

Hourly daemon tick: `current_period_start + reset_period < now` → `budget_reset` audit event, zero spend, advance start.

### Hooks integration

Wire into the existing `HookRegistry`:

| Hook point | Behavior |
|---|---|
| `BeforeLlmCall` | Budget pre-flight |
| `AfterLlmCall` | Budget reconcile |
| `PreToolUse` | Pass-through (tier categorization for audit only — no prompts; CLAUDE.md invariant) |
| `PostToolUse` | Audit categorization |

### `/status` row

```
Budgets · $0.84 / $5 daily · pace ~$3.10/day
```

No `/budget` command.

### Migration

- Existing per-provider call sites in `crates/core/src/llm/` get refactored behind `Provider` impls. Public surface from `core::Agent` calls `Provider::call`, not `OpenAiClient::chat`.
- Existing `[credentials]` table keeps owning API keys (no vault). Provider config refers to credential names.

## Out of scope

- `SovereignCloud` category, attestation gate, classification matrix, FIPS 140-3, credential-classification ceiling — all v3.
- Per-workspace budgets — no workspaces in v1.
- Live mid-stream budget cancellation — explicitly forbidden by `approvals.md` (wastes spent tokens, produces broken state).
- Rate-card auto-refresh on a schedule — opt-in command only in v1.

## Done when

- Every LLM call in the codebase routes through `Provider::call`.
- All 8 provider backends (OpenAI, Anthropic, Gemini, Groq, DeepSeek, OpenRouter, Ollama, Claude Code CLI) have `Provider` impls and pass per-provider integration tests (live or VCR fixtures).
- Adding a BYOEndpoint via `borg providers add --base-url ... --api-key-env ...` and chatting against it works without touching code.
- Budget pre-flight refusal works: daemon at $19.50/$20, $0.80 estimate → call refused, `budget_hit` event written, spend unchanged.
- Budget post-flight overshoot works: $19.80/$20 + actual $0.30 → call completes, spend = $20.10, next call refused.
- Replayed budget computation from the audit chain produces identical USD totals across runs (rate-card determinism test).
- `borg providers refresh-rates` updates rows with new `effective_at`; old rows preserved for replay.

## Risks to watch

- **Rate-card drift.** A provider silently changing prices breaks deterministic replay. The `effective_at` design handles this *if* refresh appends rather than mutates — enforce that in code.
- **Token-count divergence.** Pre-flight estimate ≠ provider actual. Always reconcile post-flight; don't trust the estimate for spend tracking, only for refusal gating.
- **Local providers and budget.** Ollama / Claude Code CLI have $0 rate cards — but the budget pipeline still runs (recording 0 spend) so audit works uniformly. Don't special-case to skip the pipeline.
