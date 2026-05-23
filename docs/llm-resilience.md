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
least-leased key then wins ties — degraded but never blocked.

### Proactive rate-limit avoidance

The pool also reads provider rate-limit response headers on every request —
both 2xx and 4xx — and skips credentials that are near-empty *before* they
return a 429. Reduces mean latency under sustained load: each window
otherwise costs one wasted 429 round-trip per key.

Headers parsed (see `crates/cortex/src/llm/rate_limit_headers.rs`):

- **OpenAI-compatible** (OpenAI, OpenRouter, Azure, Groq, xAI, DeepSeek,
  OpenAI/Copilot OAuth, Custom endpoints): `x-ratelimit-{limit,remaining,reset}-{requests,tokens}`.
  Reset accepts duration suffix strings (`6m0s`, `1h30m`, `250ms`) or
  epoch s/ms integers. Gemini is intentionally NOT in this list since
  M4 — it runs on the native transport with Gemini-specific quota
  headers.
- **Anthropic** (Anthropic API + ClaudeCode OAuth):
  `anthropic-ratelimit-{requests,tokens,input-tokens,output-tokens}-{limit,remaining,reset}`.
  Reset values are RFC 3339 timestamps. When both `input-tokens` and
  `output-tokens` are reported, the combined `tokens` bucket uses whichever
  is more constrained — that's what'll trip first.
- **Bedrock / ClaudeCli / Ollama**: not parsed (no standard headers; local
  or AWS-signed).

Skip rule on `pick()` / `acquire_lease()`:

```
remaining_pct < llm.proactive_rate_limit_threshold_pct  (default 5; 0 disables)
AND reset_at  > now + llm.proactive_rate_limit_min_window_secs  (default 5s)
```

Below the `min_window` we keep using the near-empty key — waiting it out is
cheaper than rotating to a sibling and losing in-flight context.

This is a hint, not a hard block. When every credential is proactively
throttled, the pool falls back to the unfiltered set so a request still
gets made — proactive avoidance is meant to reduce the *expected* 429
count, not block requests outright.

Snapshots are held in memory only. They expire well within their own
bucket reset window (seconds to minutes), so persisting them across
restart would just mean acting on stale data on first boot.

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
fallback engages). Source: `classify_402` in `crates/cortex/src/llm/llm_error.rs`.

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

| Setting                                       | Default      | Effect                                                                          |
| --------------------------------------------- | ------------ | ------------------------------------------------------------------------------- |
| `llm.credential_strategy`                     | `fill_first` | Selection algorithm.                                                            |
| `llm.credential_cooldown_secs`                | `60`         | Default cooldown when no `Retry-After`.                                         |
| `llm.max_concurrent_per_credential`           | `1`          | Soft lease cap per credential.                                                  |
| `llm.proactive_rate_limit_threshold_pct`      | `5`          | Skip a credential when observed bucket headroom falls below this %. `0` disables. |
| `llm.proactive_rate_limit_min_window_secs`    | `5`          | Don't proactively rotate when reset is within this many seconds (wait it out).  |

### OAuth sources

The pool seeds OAuth-capable credentials from on-disk stores when the
provider matches. Each source is read at pool-load time and on every
`borg auth pair <kind>` call.

| Kind          | Provider             | File                              | Auto-seed |
| ------------- | -------------------- | --------------------------------- | --------- |
| `claude_code` | `ClaudeCodeOAuth`    | `~/.claude/.credentials.json`     | yes       |
| `codex`       | `CodexOAuth`         | `~/.codex/auth.json`              | yes       |
| `qwen_cli`    | `Custom` (Qwen)      | `~/.qwen/oauth_creds.json`        | no, via `borg auth pair qwen` |

Expired access tokens are refreshed transparently before each request via
the per-provider OAuth endpoint (60-second skew window). Refresh failures
cool the credential for 5min — not removed, since the refresh server may
be flaky. Borg then re-picks a healthy sibling credential from the same
pool when one exists; otherwise the request falls through to the normal
provider-fallback path rather than sending a known-stale access token.
Concurrent refreshes against the same id are serialized by a per-credential
async lock so the single-use refresh_token can't be double-spent. After a
successful refresh the rotated `refresh_token` is written back to the
source file (atomic rename) so a parallel CLI consumer or the next daemon
reload doesn't replay the burnt token. Per-credential `base_url` overrides
are validated against an https-or-loopback-http allowlist at discovery and
`borg auth pair` entry; unsafe URLs are rejected rather than silently
honored.

GitHub Copilot is intentionally out of scope today — the discovery flow
needs a `gh auth token` shell-out plus a token-exchange round-trip, and
shipping a stub that always returns "no credentials" would mislead
operators. `Provider::CopilotOAuth` remains for header construction so
hand-pasted Copilot tokens still work via `borg auth add`.

### Custom OpenAI-compatible endpoints

`Provider::Custom` credentials carry a per-credential `base_url` that
overrides the provider's default. Use this for self-hosted vLLM, Ollama-
compat servers, or any OpenAI wire-format endpoint. Wire via `borg auth
add --base-url <url> --name <name> <provider>`; the suppression model
keys on `name`, not `base_url`, so port flips on a self-hosted endpoint
survive operator removal/re-add.

### Azure OpenAI

`Provider::AzureOpenAi` rides the standard credential pool with one key
difference: the wire URL is *not* static. It is composed at `LlmClient::new`
time from three settings — `llm.azure_resource`, `llm.azure_deployment`,
`llm.azure_api_version` — and stashed into `llm_config.base_url` so the
rest of the request path treats Azure identically to any other
OpenAI-compatible host. Missing or malformed settings surface as a clear
config error at construction (named-setting message from
`compose_azure_openai_url`), not as a runtime 4xx — the rule is "borg
refuses to start" over "first request 400s." A `.` in `azure_resource`
would otherwise produce a subdomain-injection vector
(`https://evil.attacker.openai.azure.com`); the composer enforces DNS-label
character classes on all three fields to close that gap.

The static-key auth header is `api-key: <key>`, not `Authorization:
Bearer`. When no Azure API key is configured, Borg uses Entra ID instead:
client credentials from `AZURE_TENANT_ID` / `AZURE_CLIENT_ID` /
`AZURE_CLIENT_SECRET`, or managed identity through Azure IMDS. Entra tokens
send `Authorization: Bearer <token>` and bypass the API-key credential pool.

### Google Vertex AI

`Provider::VertexAi` uses GCP OAuth access tokens from
`GOOGLE_APPLICATION_CREDENTIALS` service-account JSON or the metadata server.
Configure `llm.vertex_project` and optionally `llm.vertex_location`
(`global` default). Gemini models use Vertex's OpenAI-compatible chat
endpoint; Claude/Anthropic model names use the Vertex Anthropic raw-predict
endpoint with `anthropic_version = "vertex-2023-10-16"` in the request body.

**`video_analyze` does not run on Vertex-Gemini.** The OpenAI-compatible
façade strips `inline_data` parts on the way to the wire, so a video would
either be silently dropped or rejected with a 400. The
`video_analyze` handler fast-fails with a clear error pointing operators
at AI Studio Gemini. Parity with AI Studio's native transport (regional
endpoint, GCP bearer auth, per-model wire routing) is captured as a
follow-up design in `docs/specs/gemini-adapter-10-vertex-followup.md` and
is intentionally out of scope for this milestone.

### Google AI Studio (Gemini native)

`Provider::Gemini` (set `llm.provider = "gemini"`, credential
`GEMINI_API_KEY`) routes through the **native** transport at
`/v1beta/models/<model>:generateContent` (non-streaming) and
`/v1beta/models/<model>:streamGenerateContent?alt=sse` (streaming),
not the OpenAI-compatible façade. The flip is enforced by
`Provider::Gemini.is_openai_compatible() == false` — the dispatcher in
`crates/cortex/src/llm/mod.rs` picks `gemini_transport()` ahead of the
openai-compat branch. Two motivations:

- **`inline_data` survives the round-trip.** The OpenAI-compat façade
  strips `inline_data` parts (`image`/`video`/`audio` base64) when
  re-rendering the request, so `vision_analyze` and `video_analyze`
  silently lost their payloads. The native path passes them through
  as `parts[].inline_data.{mime_type,data}` exactly as authored.
- **Gemini-specific request fields work.** `thinkingConfig`,
  `safetySettings`, `cachedContents`, and `responseMimeType` are
  rejected by the compat façade. The native transport forwards them
  verbatim.

Override the base URL for self-hosted relays or wiremock-based tests
with `BORG_GEMINI_BASE_URL` (priority: env > `llm.base_url` > default).
Function calls receive synthetic IDs (`gem_call_<name>_<index>`) since
Gemini doesn't emit one; the ID stays stable so the next user turn's
`functionResponse` matches by name + ordinal.

Cost attribution: known model rates (`gemini-2.5-pro`,
`gemini-2.5-flash`, `gemini-2.5-flash-lite`, `gemini-2.0-flash`) are
in `llm::pricing::GEMINI_RATES` (≤200K-context tier). Unknown models
emit `cost_source = "unknown"` with no `cost_usd`.

Live-LLM lane: `cargo test -p cortex --features live-llm --test
live_network_integration -- live_gemini_native_` exercises the native
transport end-to-end. Operator-run only — requires both
`BORG_ALLOW_LIVE_TESTS=1` and `GEMINI_API_KEY`; the standard hermetic
suite never hits the AI Studio API.

### Fallback deferral

Before walking the `llm.fallback` chain on a 429/402, the pool is asked
`may_recover_within(5min)` — true only when the pool has at least two live
credentials and at least one cooling credential will recover within the
window. Deferral keeps the request on the primary provider so a sibling key
gets a chance, rather than burning budget on a degraded fallback while the
primary is still draining. Single-credential pools never defer fallback on
429/402 — there is nothing to rotate to. Permanently failed (tombstoned)
entries and out-of-window cooldowns also don't defer.

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
