# LLM Providers — v1

Status: design, in active rework. Companion to [`architecture-redesign.md`](architecture-redesign.md), [`auth-and-onboarding.md`](auth-and-onboarding.md).

## Goal

Define how Borg picks, configures, and constrains LLM providers in v1. v1 is single-mode (no Sentry), no workspaces, hobbyist-shaped: one global default provider, optional fallback chain, deterministic token accounting via versioned rate cards.

Sovereign-cloud category, attestation gates, classification matrix, FIPS-validated TLS, and per-workspace credential pinning are v3 additions when multi-tenancy lands and per-workspace provider floors become load-bearing.

## Non-goals

- **Cross-provider fallback as the default.** Hobbyist gets one provider; if it's down, the agent surfaces an error rather than silently switching to a more expensive or differently-aligned provider. Power users can opt into chains.
- **Universal tool-schema translator.** Each provider keeps its own tool-call format; the abstraction normalizes inbound/outbound, doesn't pretend they're identical.
- **Hosted billing or relay.** Borg never proxies through an Anthropic-owned or Borg-owned cloud. Tokens flow client → daemon → provider, full stop.
- **Provider auto-discovery.** No "we found Ollama on your network" prompts. Explicit configure.

## Locked decisions (v1)

| # | Decision |
|---|---|
| 1 | Three provider categories: `PublicCloud` / `Local` / `BYOEndpoint`. |
| 2 | One global default provider. Optional fallback chain (existing current-Borg pattern). |
| 3 | Token accounting is the kernel-side budget primitive (USD displayed, tokens stored). |
| 4 | Per-provider rate card is a `provider_rate_cards` row, versioned by `effective_at`. |
| 5 | Air-gap mode = `[providers] outbound_allowed = false` rejects every provider whose category is not `Local`. |
| 6 | TLS to providers uses `rustls` + `aws-lc-rs` standard provider; per-endpoint SPKI pin where configured. |
| 7 | Tool-schema translation is per-provider plugin code, not a universal mapper. |
| 8 | Streaming-replay strategy is per-provider — each declares whether it supports resume on disconnect. |
| 9 | Default-mode hobbyists may opt in to a manual rate-card refresh via `borg providers refresh-rates` (signed manifest fetched over TLS, verified before write). Never on a timer, never auto-fetched. |

---

## Categorization

### Category set (v1)

| Category | Examples | v1 |
|---|---|---|
| `PublicCloud` | OpenAI, Anthropic public, Gemini, Groq, DeepSeek, OpenRouter | Allowed |
| `Local` | Ollama, llama.cpp, vLLM (local), LM Studio | Allowed |
| `BYOEndpoint` | Any OpenAI-compatible URL (on-prem vLLM cluster, internal API gateway) | Allowed |

The TUI onboarding wizard collapses categories — you pick a category, then a provider within it. v1 does not surface the v3 `SovereignCloud` category at all.

### Provider as global setting

v1 has no workspaces, so providers live at the daemon-global level:
- `settings.providers.default_id` — the active provider
- `settings.providers.fallback_chain` — optional ordered list (existing current-Borg pattern)

v3 adds `workspaces.provider_id` (per-workspace override) when workspaces exist.

```sql
CREATE TABLE providers (
  id              TEXT PRIMARY KEY,        -- "openai-prod", "ollama-local"
  category        TEXT NOT NULL,           -- "public_cloud" | "local" | "byo_endpoint"
  display_name    TEXT NOT NULL,
  endpoint_url    TEXT NOT NULL,
  auth_credential TEXT,                    -- credentials store ref
  trust_root_id   TEXT,                    -- nullable; BYO can pin a self-signed cert
  rate_card_id    TEXT REFERENCES provider_rate_cards(id),
  capabilities    TEXT NOT NULL,           -- JSON: {"streaming": true, "tools": true, "vision": false, ...}
  enabled         INTEGER NOT NULL DEFAULT 1,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL
);

CREATE TABLE provider_rate_cards (
  id              TEXT PRIMARY KEY,        -- "openai-2026-04", "anthropic-2026-04"
  provider_kind   TEXT NOT NULL,           -- "openai" | "anthropic" | "ollama" | ...
  model           TEXT NOT NULL,
  prompt_per_mtok TEXT NOT NULL,           -- USD as decimal string for precision
  output_per_mtok TEXT NOT NULL,
  cache_read_per_mtok TEXT,
  cache_write_per_mtok TEXT,
  effective_at    INTEGER NOT NULL,
  source          TEXT NOT NULL            -- "borg_release_2026.04" | "manifest_refresh_<ts>"
);
```

`attestation`, `credential_classification`, `provider_floor` columns are v3 additions; v1 schema omits them entirely (added when v3 lands, schema migration is additive).

---

## Onboarding flow

### Hobbyist (fresh install)

```
Pick your model:
  [1] OpenAI / Anthropic / Gemini    (cloud, fastest, requires API key)
  [2] Ollama / llama.cpp             (local, runs on this machine)
  [3] Custom endpoint                (advanced)

> 1
  Provider: ( ) OpenAI  ( ) Anthropic  ( ) Gemini  ( ) Groq  ( ) DeepSeek  ( ) OpenRouter
  > OpenAI
  API key: [paste]
  Default model: gpt-4o-mini ▼
  ✓ Saved as "openai-prod" — set as global default
```

One screen, one decision. Stored API key goes through the existing `[credentials]` table. Editing the active provider lives in `/settings → LLM`.

---

## Air-gap mode

`[providers] outbound_allowed = false` (settable via `/settings → Network → Air-gap`):

- Onboarding wizard hides `PublicCloud` and `BYOEndpoint` *unless* the BYO endpoint resolves to an RFC1918 / link-local / loopback address.
- Runtime: provider category check; non-`Local` providers refused with a clear error (`AirGapPolicyViolation`).
- Air-gap is a *posture signal* — it composes well with Boosted state per `security-posture.md` (no public-egress = stricter posture).

The `borg update` mechanism still works in air-gap — operator manually transfers the binary + signed rate-card update to the daemon's host.

---

## Token accounting & rate cards

### Why rate cards in the DB

1. **Determinism.** Budget enforcement (`approvals.md`) needs a stable USD↔token rate at the moment of the call. Live-fetching a rate from a vendor API turns budget math into a network dependency.
2. **Audit replay.** "Why did this turn cost $0.42 last quarter?" needs the rate that was effective then, not today's. Rate cards are versioned by `effective_at`.

Rate cards ship with the binary. `borg update` may include rate-card refreshes; air-gapped operators apply them manually. Auto-fetching from a vendor pricing API is explicitly forbidden — adds an outbound dependency, breaks air-gap, and creates a tampering vector.

### Opt-in refresh (v1)

Hobbyists who want fresher pricing without a full `borg update` can run `borg providers refresh-rates`. The command fetches a signed rate-card manifest from the Borg release host, verifies the signature against the embedded release key, diffs the new card against the current row, and writes a new `provider_rate_cards` row with `effective_at = now`. Existing rows are kept (versioned by `effective_at`) so audit replay stays deterministic.

The command is **opt-in only** — never invoked from a timer, hook, or self-healing job.

### Per-call accounting

Every LLM call writes a `provider_call` event into the chain:

```rust
pub struct ProviderCallMetadata {
    pub provider_id: String,
    pub model: String,
    pub rate_card_id: String,        // which rate card priced this call
    pub prompt_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub usd_cost: String,            // decimal string, computed at write time
    pub principal_id: PrincipalId,
    pub turn_id: TurnId,
    pub duration_ms: u32,
}
```

Budget enforcement (`approvals.md`) reads rolling sums from this event stream. `/status` `Cost` row reads the same.

(v3 adds `workspace_id` when workspaces exist.)

---

## Streaming & resume

Per-provider declared via `capabilities.streaming_resume`:

| Provider | Streaming | Resume on disconnect |
|---|---|---|
| OpenAI / Anthropic / Gemini | ✓ | ✗ (no native resume — kernel re-issues from last delivered token) |
| Ollama / llama.cpp / vLLM | ✓ | ✓ (server holds the generation; reconnect rejoins) |
| BYO OpenAI-compat | ✓ | provider-declared |

The kernel exposes a single resumable stream to *clients*, regardless of provider — the provider plugin handles the upstream gap. Bookkeeping lives in the resumable event log.

---

## TLS to providers

| Setting | Behavior |
|---|---|
| Default | `rustls` + `aws-lc-rs` (standard). System trust root + provider's expected SPKI pin (when configured). |
| `BYOEndpoint` with self-signed cert | Requires explicit `trust_root_id` row (PEM-pinned) or SPKI pin. No fallback to system trust. |

(v3 Sentry adds FIPS-validated provider, removes system-trust fallback entirely, requires SPKI pin on every BYO endpoint.)

---

## Provider plugin layer

Each provider is implemented as a compiled-in Rust module conforming to:

```rust
pub trait Provider: Send + Sync {
    fn category(&self) -> ProviderCategory;
    fn capabilities(&self) -> &ProviderCapabilities;

    async fn chat(
        &self,
        req: ChatRequest,
        cx: &CallContext,
    ) -> Result<BoxStream<'static, ChatEvent>, ProviderError>;

    fn translate_tool_calls(
        &self,
        tools: &[ToolSchema],
    ) -> Result<serde_json::Value, ProviderError>;

    fn parse_tool_call_response(
        &self,
        raw: &serde_json::Value,
    ) -> Result<Vec<ToolCall>, ProviderError>;

    fn count_tokens(
        &self,
        messages: &[Message],
        model: &str,
    ) -> Result<TokenCount, ProviderError>;
}
```

v1 compiled-in implementations: `OpenAiProvider`, `AnthropicProvider`, `GeminiProvider`, `OllamaProvider`, `LlamaCppProvider`, `VllmProvider`, `OpenRouterProvider`, `DeepSeekProvider`, `GroqProvider`, `BYOEndpointProvider` (treats endpoint as OpenAI-compatible).

WASM/dynamic plugin loading for providers is **out of scope** — same reasoning as verifier plugins in `auth-and-onboarding.md`. Provider compromise = data exfiltration; you can't sandbox a provider's truthiness about where the data went.

---

## Test posture

- **Air-gap enforcement.** `outbound_allowed = false` + non-`Local` provider call refused at runtime.
- **Rate card determinism.** Same call priced today and replayed in 6 months produces the same USD cost — rate card is pinned by `effective_at`.
- **Token accounting accuracy.** Counted prompt + output tokens within ±2% of provider's reported usage across 100 sample calls per provider.
- **TLS pinning.** BYO endpoint with mismatched SPKI pin refuses to handshake; matching pin succeeds.
- **Streaming resume.** `OllamaProvider` resume after socket drop reconnects to the same generation; `OpenAiProvider` resume re-issues from last delivered token.
- **Tool-schema translation.** Each provider's `translate_tool_calls` round-trips the canonical tool schema; parse output normalizes back to canonical `ToolCall`.
- **Rate-card refresh signature.** `borg providers refresh-rates` with a tampered manifest refuses to write; with a valid manifest writes a new versioned row and preserves the old one.

---

## Deferred to v3

When v3 lands, this doc gains:

- **`SovereignCloud` category** — Bedrock GovCloud, Azure Gov OpenAI, GCP Assured Workloads.
- **Attestation gate** — `provider.attestation` field (`fedramp_high`, `il5`, etc.) intersected with workspace `classification` to refuse non-conforming calls.
- **Per-workspace provider pinning** — `workspaces.provider_id` override, with classification × attestation matrix enforcement.
- **`credential_classification` ceiling** — a Sentry workspace's classification gates which providers can serve it (cross-classification cache refusal).
- **FIPS-validated TLS** — `rustls` + `aws-lc-rs` FIPS provider, refuses to handshake without it in Sentry mode.
- **Trust-root-only mode** — Sentry refuses system-trust fallback; every provider must have a `trust_root_id` row.
- **Air-gap-and-Sentry composition** — both can be on simultaneously, both contribute Hardened-bucket signals.

The v1 schema (`providers`, `provider_rate_cards`) is forward-compatible — v3 additions are nullable columns and new categories, not migrations of v1 data.

---

## Open questions (v1)

1. **Rate card distribution cadence.** Bundled with `borg update` is the default; opt-in `refresh-rates` is the bridge. Revisit if vendors change pricing more than once per quarter.
2. **Failover within a single agent state.** v1 supports the existing fallback-chain mechanism from current Borg. v3 adds per-workspace override, which is when "primary OpenAI, fallback Anthropic" composes well with classification.
3. **Per-tool model routing.** Some tools (long-context summarization) want a different model than the main loop. Defer.

---

## References

- Companion: [`architecture-redesign.md`](architecture-redesign.md) — Layer 1 storage, capability protocol
- Companion: [`auth-and-onboarding.md`](auth-and-onboarding.md) — credential store, listener model
- Companion: [`approvals.md`](approvals.md) — budget enforcement reading provider call events
- Existing fallback-chain code: `crates/core/src/config/llm.rs`
