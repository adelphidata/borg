# LLM resilience

Three layers stand between a transient provider failure and a user-visible
error. Each turn, the agent walks them inside-out:

1. **Credential pool** — per-provider rotation across multiple keys with
   cooldown TTL, strategy selection, and persisted state.
2. **Provider failover** — switch to the next provider when a pool exhausts
   or a slot enters cooldown (`llm.fallback`).
3. **Aux chain** — a separate client (`llm.aux`) for background tasks so a
   primary outage cannot kill nightly cron.

## Credential pool

### State model

Each provider owns one pool. Each entry carries:

| Field                  | Purpose                                                   |
| ---------------------- | --------------------------------------------------------- |
| `id`                   | Stable 6-hex identifier for operator commands.            |
| `label`                | Operator-friendly tag (env var name, config label).       |
| `source`               | `manual` / `env` / `config`. Drives the suppression model. |
| `fingerprint`          | 16-hex BLAKE3(key) — row identity. Plaintext is never stored. |
| `use_count`            | Bumped at pick time.                                      |
| `last_status`          | `ok` / `exhausted`.                                       |
| `last_error_*`         | Code, reason, message excerpt — for `borg auth list`.     |
| `last_error_reset_at`  | When the current cooldown expires.                        |
| `removed_at`           | Soft-delete tombstone (auth-class failure or `borg auth remove`). |

### Transitions

```
       ┌────┐                                ┌────────┐
       │ Ok │ ── 401 / 403 (auth fail) ────► │Removed │ (sticky soft-delete)
       └─┬──┘                                └────────┘
         │
         │ 402 (transient) / 429
         ▼
   ┌──────────┐
   │Exhausted │ ◄── cooldown_until = max(Retry-After, llm.credential_cooldown_secs)
   └────┬─────┘
        │ cooldown expires OR report_success
        ▼
       Ok
```

Other failure reasons (Overloaded / Timeout / Format / ContextOverflow /
PayloadTooLarge / ModelNotFound / Unknown) are no-ops on the pool — they're
transport or payload concerns, not credential concerns.

### Strategies

| Strategy      | Pick rule                                                       |
| ------------- | --------------------------------------------------------------- |
| `fill_first`  | Lowest-priority available key (default).                        |
| `round_robin` | Rotate to the next key after each pick.                         |
| `least_used`  | Lowest `use_count`; ties broken by priority.                    |
| `random`      | Uniform random across available keys.                           |

Set via `llm.credential_strategy` (`/settings` → Resilience).

### Concurrency

Each `pick` / `acquire_lease(None)` holds a per-credential lease counter,
bounded by `llm.max_concurrent_per_credential` (default 1). Concurrent
requests fan out across keys until every key is at the cap; the
least-leased key then wins ties (Hermes parity — degraded but never blocked).

### Retry-After parsing

Order of precedence:

1. `Retry-After` HTTP header (set by upstream).
2. `error.retry_after` JSON field.
3. Free-form body hints: `quotaResetDelay`, `resets_at`, `reset_at`,
   `try again in N (s|min|h)`, `retry after N seconds`. Includes
   OpenRouter's `error.metadata.raw` wrapping for upstream-relayed errors.
4. Configured default (`llm.credential_cooldown_secs`, default 60s).

All durations are clamped to `[1s, 24h]`.

### 402 disambiguation

A 402 with usage/quota markers AND a transient signal (try-again, reset,
window) is classified as `RateLimit` (cools, retryable). A 402 with only
hard exhaustion language is classified as `Billing` (fatal at this slot,
fallback engages). Source: `classify_402` in `crates/cortex/src/llm_error.rs`.

### Persistence

Two SQLite tables (see `crates/cortex/src/db/migrations.rs`):

- `credential_pool` — row per (provider, fingerprint). Plaintext keys never
  written. State (use_count, last_status, cooldown_until, removed_at)
  survives restart.
- `credential_suppressions` — operator removals via `borg auth remove`
  write a row keyed by source so re-seeding from env/config doesn't undo
  the removal on next start.

The fingerprint (16-hex BLAKE3) is one-way and truncated — non-secret. It
re-attaches DB state to live keys at load time. Keys whose source
disappeared between runs are pruned (env var unset, config block removed).

### Operator surface

The pool exposes an admin API in Rust today
(`CredentialPool::{entries, add_entry, remove_index, resolve_target,
reset_cooldowns, set_strategy, pick_force}`). Wiring this through gRPC
admin RPCs and a `borg auth list/add/remove/reset/set-strategy` clap
subcommand is a follow-up — track it in `docs/ROADMAP.md`. Until then,
operators can:

- Edit `llm.api_keys` and `llm.credential_strategy` via `/settings`.
- Inspect state directly via `sqlite3 ~/.borg/borg.db "SELECT * FROM credential_pool"`.
- Restart the daemon to clear cooldowns (or clear via SQL UPDATE).

### Settings

| Setting                                  | Default      | Effect                                  |
| ---------------------------------------- | ------------ | --------------------------------------- |
| `llm.credential_strategy`                | `fill_first` | Selection algorithm.                    |
| `llm.credential_cooldown_secs`           | `60`         | Default cooldown when no `Retry-After`. |
| `llm.max_concurrent_per_credential`      | `1`          | Soft lease cap per credential.          |

### OAuth sources

The pool seeds OAuth-capable credentials from on-disk stores when the
provider matches. Each source is read at pool-load time and on every
`borg auth pair <kind>` call.

| Kind          | Provider             | File                              |
| ------------- | -------------------- | --------------------------------- |
| `claude_code` | `ClaudeCodeOAuth`    | `~/.claude/.credentials.json`     |
| `codex`       | `CodexOAuth`         | `~/.codex/auth.json`              |
| `qwen_cli`    | `Custom` (Qwen)      | `~/.qwen/oauth_creds.json`        |
| `copilot`     | `CopilotOAuth`       | resolved via `borg auth pair`     |

Expired access tokens are refreshed transparently before each request via
the per-provider OAuth endpoint (60-second skew window). Refresh failures
cool the credential for 5min — not removed, since the refresh server may
be flaky. Concurrent refreshes against the same id are serialized by a
per-credential async lock so the single-use refresh_token can't be
double-spent.

### Custom OpenAI-compatible endpoints

`Provider::Custom` credentials carry a per-credential `base_url` that
overrides the provider's default. Use this for self-hosted vLLM, Ollama-
compat servers, or any OpenAI wire-format endpoint. Wire via `borg auth
add --base-url <url> --name <name> <provider>`; the suppression model
keys on `name`, not `base_url`, so port flips on a self-hosted endpoint
survive operator removal/re-add.

### Fallback deferral

Before walking the `llm.fallback` chain on a 429/402, the pool is asked
`may_recover_within(5min)` — true when at least one cooling credential
will recover within the window. Deferral keeps the request on the
primary provider so a sibling key gets a chance, rather than burning
budget on a degraded fallback while the primary is still draining.
Permanently failed (tombstoned) entries and out-of-window cooldowns
don't defer — fallback engages as before.

## Provider failover

When the active provider's pool exhausts (or a non-credential error makes
the slot unusable), the failover loop moves to the next entry in
`llm.fallback`. Each slot tracks its own exponential-backoff cooldown
(`ProviderCooldown`) so a recently-burned slot is skipped on the next
provider scan. See `try_failover_provider` in
`crates/cortex/src/llm/mod.rs`.

Slot order is config order. `llm.fallback` is a JSON array of
`{ provider, model, api_keys, ... }` blocks — onboarding seeds it with
a same-provider cheap-tier sibling by default.

## Aux chain

`llm.aux` materializes its own `LlmClient` via `LlmClient::for_auxiliary`
with isolated `provider_slots`. Aux-side cooldown state cannot bleed into
the primary client. Each provider, however, shares a single pool across
primary and aux callers — that's intentional: one provider, one pool,
regardless of who's calling it. (If isolation per-caller is needed
later, the natural escape hatch is an `aux:provider` pool key prefix.)

## Known limitations

- The CLI/gRPC operator surface is not wired yet; the underlying Rust API
  is complete and unit-tested. Tracked as follow-up.
- Pool state is shared per `provider` across primary and aux clients. Use
  the future `aux:provider` pool-key prefix when full isolation is needed.
- Fingerprints are 128-bit truncated BLAKE3. Collisions are unrealistic
  (~2⁻⁶⁴ per pair) but acknowledged; duplicate-label warnings during pool
  load surface any practical collision.
