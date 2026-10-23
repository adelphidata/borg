# Memory

Borg uses a three-layer memory model:

| Layer | Storage | Lifetime | Prompt role |
|------|---------|----------|-------------|
| **Working memory** | In-memory (`ShortTermMemory`) | Current session only | Dynamic suffix via `<working_memory>` |
| **Knowledge files** | `~/.borg/memory/*.md` and `~/.borg/memory/scopes/<scope>/*.md` | Persistent until edited/removed | Stable prompt block via `<knowledge_files>` |
| **Long-term memory** | `memory_entries` plus embeddings/chunks in `borg.db` | Persistent across sessions | Stable prompt block via `<long_term_memory>` |

`IDENTITY.md` and `HEARTBEAT.md` remain separate persistent files with their own loading rules.

## Working memory

Session-scoped working memory accumulates facts during the conversation:

- Fact categories: `Decision`, `Preference`, `TaskOutcome`, `CodeFact`, `Correction`
- Budget-aware: oldest facts are evicted when the token budget is exceeded
- Rendered as `<working_memory>` only when non-empty

Working memory is never edited directly. On session end it flushes to a daily long-term entry, which later consolidation jobs can turn into curated durable memory.

## Knowledge files

Knowledge files are curated markdown notes stored under `~/.borg/memory/`.

- `~/.borg/memory/MEMORY.md` is treated as the top-level index when present
- Additional topic files can live alongside it
- Scoped knowledge files live under `~/.borg/memory/scopes/<scope>/`
- Knowledge files are loaded by relevance, not all at once

The loader always tries to include `MEMORY.md` first, then adds the most relevant markdown files for the current query until the knowledge-file token budget is exhausted.

Knowledge files are also included in `memory_search`, so they can be recalled on demand even if they were not auto-loaded into the prompt.

### Obsidian workflow

Obsidian support is optional and layered on top of the existing knowledge-file store.

- `~/.borg/memory/` is Borg's primary Obsidian-facing markdown surface
- Borg does not create a separate `~/.borg/vault/` directory
- Users who want an Obsidian workflow should point Obsidian at the relevant markdown surface instead of expecting a storage migration
- `MEMORY.md` remains the top-level curated map for markdown knowledge files

This does not change the long-term memory backend. SQLite remains the source of truth for `write_memory` and `read_memory`.

When Borg maintains markdown intended for Obsidian, it should prefer deterministic managed regions inside mixed-ownership files:

```md
<!-- borg:managed:start name=summary -->
... Borg-managed content ...
<!-- borg:managed:end name=summary -->
```

Content inside those markers may be rewritten by Borg. Content outside them should be preserved unless the file is explicitly agent-owned.

## Long-term memory

Long-term memory is stored in the database as named entries with a `scope + name` key.

- `write_memory` and `read_memory` operate on these entries
- Embeddings and chunks are generated from long-term memory entries for semantic retrieval
- The memory loader always tries to include the `INDEX` entry first
- Remaining entries are loaded by semantic ranking when embeddings are available, otherwise by recency

### Scopes

Borg resolves long-term memory and knowledge files using:

1. `global`
2. the active configured scope, if `memory.memory_scope` is set

Project-scoped entries such as `project:abc` are first-class. Scoped entries participate in both prompt loading and `memory_search`.

## Prompt loading

Each turn, Borg assembles memory in this order:

1. Long-term memory entries in `<long_term_memory trust="stored">`
2. Knowledge files in `<knowledge_files trust="curated">`
3. Active retrieved memory in `<active_memory trust="retrieved">`, when enabled
4. Session working memory in `<working_memory>`

`<active_memory>` is a per-turn search block keyed off the latest user message. It can pull from long-term memory, knowledge files, and session transcripts.

## Memory search

`memory_search` can search:

- Long-term memory entries
- Knowledge files
- Past session transcripts when `sources=["messages"]`

When searching long-term memory, Borg uses hybrid retrieval:

- BM25 keyword matching
- Vector similarity from embeddings
- Optional MMR reranking for diversity

When no multi-word query hits are found, Borg falls back to term-level FTS matches with a score discount so phrase matches still rank higher.

Knowledge files use lightweight relevance scoring over filenames and content and are returned in the same result stream, labeled with a `knowledge:` prefix internally.

## Writing memory

The agent uses `write_memory` to create or append long-term memory entries:

```json
{
  "filename": "user-preferences",
  "content": "# User Preferences\n\n- Prefers concise answers\n- Timezone: PST",
  "append": false,
  "scope": "global"
}
```

- `filename`: entry name (`.md` suffix is stripped for backward compatibility)
- `content`: text to write
- `append`: append instead of overwrite
- `scope`: `global` by default, or a scope such as `project:<id>`

`write_memory` does not write markdown knowledge files. Those remain plain files under `~/.borg/memory/` for curated, Obsidian-friendly notes.

### Safety checks

All long-term memory writes are scanned for prompt-injection patterns before they are persisted:

- Prompt override phrases
- Secret-exfiltration patterns
- Dangerous invisible Unicode

Writes are rejected when these patterns are detected.

### Size limits

`write_memory` rejects entries larger than 20,000 tokens and warns at 8,000 tokens. This prevents oversized entries from being silently skipped later by prompt-budget loading.

## Reading memory

`read_memory` fetches a long-term memory entry by name and optional scope:

```json
{
  "filename": "user-preferences",
  "scope": "global"
}
```

It returns the entry content or a friendly not-found message.

## Consolidation

Memory consolidation runs as scheduled tasks:

### Nightly

Reviews the day’s session output, extracts durable facts, and appends or creates long-term topic entries without duplicating existing knowledge.

### Weekly

Reviews long-term memory for duplicates, stale information, and merge opportunities, and prunes the embedding cache.

## Pluggable memory providers

Borg's long-term memory tier routes through a `MemoryProvider` trait. The
three model-visible tools (`write_memory`, `read_memory`, `memory_search`)
and the per-turn `<active_memory>` injection block use the chosen provider.
The stable-prefix `<long_term_memory>` block in the system prompt currently
still loads from local SQLite regardless of provider — see the "Memory
provider — full prompt-block routing" entry in `docs/ROADMAP.md`. External
provider content still reaches the model via `<active_memory>` (per-turn
hybrid search) and on-demand via `read_memory` / `memory_search`.

The default is `native` — the SQLite-backed store described above.
Operators can swap in an external agent-memory service via:

```sh
borg settings set memory.provider mem0
```

Available providers:

| Provider     | Storage                                     | When to pick it |
|--------------|---------------------------------------------|-----------------|
| `native`     | SQLite at `~/.borg/borg.db`                 | Default. Offline, deterministic, no third-party dependency. |
| `mem0`       | Cloud service with server-side fact extraction | You want the model's writes distilled into facts automatically and shared across devices. |
| `honcho`     | Cloud service with peer/session model + dialectic Q&A | You want a chat-history-aware memory layer that exposes a Q&A endpoint over your past turns. |
| `openviking` | Self-hosted or cloud hierarchical context store | You want a `viking://account/user/agent/scope/name` URI scheme and operator-controlled deployment. |
| `letta`      | Stateful-agent service (MemGPT lineage) with hierarchical memory tiers | You want Letta's archival memory plus always-in-context **core-memory blocks**, scoped to a Letta agent. |
| `zep`        | Temporal knowledge-graph service with enriched session summaries | You want graph-based recall across sessions plus server-built **session summaries** surfaced at retrieval time. |

Switching providers does **not** migrate existing data. The native SQLite
store remains intact while another provider is active; flipping back via
`borg settings unset memory.provider` returns to it instantly.

### Shared invariants

Every provider — including external ones — sees the same:

- **Prompt-injection scan** before each write (override phrases, exfil,
  invisible Unicode). External providers cannot accept a tainted payload
  even if the upstream service would.
- **20,000-token write cap.** Oversized entries are rejected at the
  dispatch layer, not silently dropped at prompt-loading time.
- **No-approval-prompts UX.** Tool calls execute through the chosen
  provider with the same blocking-on-error semantics as the native path.
- **Plan-mode gating.** `write_memory` stays blocked in Plan mode
  regardless of provider; `read_memory` and `memory_search` are allowed.

These live in `crates/cortex/src/memory/provider/mod.rs::write_guarded`
so a new provider impl cannot bypass them.

### Configuration

```sh
# Pick the backend (any of: native, mem0, honcho, openviking, letta, zep)
borg settings set memory.provider mem0

# Mem0 — set the API key (resolved through the secret-ref shape used
# elsewhere in the config; `${SECRET:NAME}` placeholders work)
borg settings set memory.mem0.api_key '{"env":"MEM0_API_KEY"}'
borg settings set memory.mem0.organization "acme"        # optional
borg settings set memory.mem0.user_id "alice"            # optional, falls back to $USER

# Honcho
borg settings set memory.honcho.api_key '{"env":"HONCHO_API_KEY"}'
borg settings set memory.honcho.app_id "borg"

# OpenViking (self-hosted endpoint required)
borg settings set memory.openviking.endpoint "https://openviking.internal"
borg settings set memory.openviking.api_key '{"env":"OPENVIKING_KEY"}'
borg settings set memory.openviking.account "acme"

# Letta (MemGPT) — memory is agent-scoped, so agent_id is required
borg settings set memory.letta.api_key '{"env":"LETTA_API_KEY"}'
borg settings set memory.letta.agent_id "agent-abc123"
borg settings set memory.letta.project "default"        # optional, Letta Cloud
borg settings set memory.letta.core_scope "core"        # optional; scope that maps to core-memory blocks

# Zep
borg settings set memory.zep.api_key '{"env":"ZEP_API_KEY"}'
borg settings set memory.zep.user_id "alice"            # optional, falls back to $USER
```

The **`letta`** backend maps most scopes onto Letta's embedding-searchable
**archival memory** (each entry carries a `borg-key: {scope}/{name}` marker
so reads/deletes stay deterministic). The configured `core_scope` (default
`core`) routes instead to Letta's always-in-context **core-memory blocks**
(`label = name`), and `memory_search` ranks matching core blocks above
archival hits.

The **`zep`** backend writes each entry as a metadata-tagged message into a
per-scope session (like `honcho`), and `memory_search` blends Zep's
user-level **knowledge-graph search** with each in-scope session's
server-built **enriched summary**.

### Provider health check

`borg doctor` reports the configured memory backend under the **Memory**
category. For external providers the interactive doctor issues a cheap
authenticated probe: a healthy provider shows `provider '<name>' reachable`,
a selected-but-unconfigured provider shows a `Warn` pointing at the missing
`memory.<provider>.api_key`, and an auth/endpoint failure surfaces the
(secret-redacted) error — so misconfiguration is visible before a write
silently falls back to native. The unattended nightly maintenance sweep runs
the same diagnostics but **skips the network probe** (reporting only
`provider '<name>' configured`), so the self-heal pass never makes outbound
calls to the memory service.

If the chosen provider's credentials are missing at daemon startup, the
registry falls back to `native` with a one-shot `tracing::warn!` in
`daemon.log` — Borg always boots. Tool results from `write_memory` and
`read_memory` are prefixed with a one-line `[note] memory.provider=…
is unconfigured; this call used the native SQLite store.` so the
operator (and the model) see that the write didn't reach the selected
external store. The notice is suppressed when the operator actually
selected `native`.

### Consolidation

The nightly / weekly consolidation passes are skipped when an external
provider is active. Those services consolidate server-side; running the
seeded-agent loop locally would burn LLM budget for no effect. The
`/heal` and `borg poke` paths surface a "delegated to <provider>" line
so operators can confirm the skip is intentional.

### Adding more providers

The trait is intentionally small (`write` / `read` / `search` / `list` /
`delete` / `consolidate`, plus an optional `health` probe). Adding a new
backend means dropping a file under `crates/cortex/src/memory/provider/`,
wiring a variant into `MemoryProviderKind`, and registering credential keys
in `config/settings_table.rs`. Session-per-scope backends can reuse the
shared `scoped_session_id` encoder. Tests should follow the wiremock
fixtures in `provider/tests.rs` — pin the request shape your impl sends,
not canned response data.

## IDENTITY.md

`IDENTITY.md` is not part of the general memory stores. It is loaded at the front of the system prompt and defines the assistant’s persistent personality and behavior.

The agent's name lives in the `user.agent_name` setting (first-run screen, Settings → Name, or `borg settings set user.agent_name …`). Changing it rewrites the name in `IDENTITY.md` (heading and `You are …,`), the `- Agent:` line of `$BORG_HOME/MEMORY.md`, and `SETUP.md` while it still exists. With no `IDENTITY.md`, the default identity uses the setting.

The owner's name lives in the `user.name` setting (first-run wizard, Settings → Name, or `borg settings set user.name …`). It is shown in the desktop and web sidebar and given to the agent on an `Owner:` line in `<environment>`. When the owner says what to call them, the agent saves it by passing `user_name` to `write_memory`; a channel message (Slack, Telegram, …) cannot set it, since its sender may not be the owner. Changing it rewrites the `- Name:` line under `## Owner` in `$BORG_HOME/MEMORY.md` and the template's `belonging to …` in `IDENTITY.md`, and nothing else: owner names are often ordinary words.

## HEARTBEAT.md

`HEARTBEAT.md` is an optional checklist used only during heartbeat turns.

## Session persistence

Conversations are saved in `borg.db` immediately for crash recovery. Session transcripts are searchable through `memory_search` when `sources=["messages"]` is requested or when active memory injection searches both memory and messages.

Working memory itself remains in RAM for the current session. Obsidian-oriented markdown artifacts should be treated as filed outputs or curated knowledge notes, not as the live short-term memory store.

## Tips

- Keep `INDEX` concise and high-signal
- Keep `MEMORY.md` as a curated map of important knowledge files
- Use scoped memory for project- or channel-specific context
- Prefer topic-sized long-term entries over one huge catch-all note
