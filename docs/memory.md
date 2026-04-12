# Memory

Borg has a two-tier memory system modeled on how human memory works: short-term working memory for the current session, and long-term persistent memory stored in SQLite.

## Architecture

| Tier | Storage | Lifetime | Loaded |
|------|---------|----------|--------|
| **Short-term** | In-memory (`ShortTermMemory` struct) | Current session only | Dynamic suffix of system prompt |
| **Long-term** | SQLite `memory_entries` table | Persistent across sessions | Stable prefix of system prompt |
| **Identity** | `~/.borg/IDENTITY.md` (file) | Persistent | Always (system prompt prefix) |
| **Heartbeat** | `~/.borg/HEARTBEAT.md` (file) | Persistent | Heartbeat turns only |

All long-term memory entries live in the `memory_entries` table with a `scope` + `name` unique key. No filesystem memory files — SQLite is the single source of truth.

## Long-term memory

### How loading works

Each turn, the agent builds a memory context within a token budget:

1. The **INDEX** entry is loaded first (always included if it fits)
2. Remaining entries are ranked by semantic similarity to the user's query, blended with recency
3. Entries are included one by one until the token budget is exhausted
4. Rendered as `<long_term_memory trust="stored">` in the system prompt stable prefix

### Semantic search

Embeddings are generated on `write_memory` and stored in SQLite. Entries are ranked by cosine similarity to the user's query, blended with recency. The system uses hybrid search combining BM25 keyword matching (30%) with vector similarity (70%).

When no results match a multi-word query, the search falls back to individual term matching (with a 0.7x score discount so term-level hits rank below phrase hits).

Auto-detects embedding provider from API keys (OpenAI -> OpenRouter -> Gemini). Falls back to recency-only ranking when no provider is available.

### Configuration

```sh
borg settings set memory.max_context_tokens 8000
borg settings set memory.embeddings.enabled true
borg settings set memory.embeddings.recency_weight 0.2    # 0.0=pure similarity, 1.0=pure recency
borg settings set memory.embeddings.bm25_weight 0.3
borg settings set memory.embeddings.vector_weight 0.7
```

## Short-term memory

Session-scoped working memory that accumulates facts during the conversation:

- **Fact categories**: Decision, Preference, TaskOutcome, CodeFact, Correction
- **Budget-aware**: Evicts oldest facts when token limit exceeded (default: 2000 tokens)
- **Active project**: Tracks the current project context from the projects tool
- Rendered as `<working_memory>` in the system prompt dynamic suffix
- Omitted entirely when empty (zero token cost)

Short-term memory is never written directly to long-term storage. Instead, it flushes to a daily log entry on session end, which the nightly consolidation job processes.

## Writing memory

The agent uses the `write_memory` tool:

```json
{
  "filename": "user-preferences",
  "content": "# User Preferences\n\n- Prefers concise answers\n- Timezone: PST",
  "append": false,
  "scope": "global"
}
```

- `filename`: entry name (`.md` suffix stripped automatically for backward compatibility)
- `content`: text to write
- `append`: if `true`, appends to existing content
- `scope`: `"global"` (default) or `"project:{id}"` for project-scoped memory

### Injection scanning

All memory writes are scanned for prompt injection patterns before persisting:

- **Prompt override**: "ignore previous instructions", "you are now", "system prompt override"
- **Exfiltration**: curl/wget with secrets, credential harvesting
- **Invisible Unicode**: zero-width spaces (U+200B), RTL overrides (U+202E), BOM (U+FEFF)

Writes containing these patterns are rejected with an error.

## Reading memory

The agent uses the `read_memory` tool:

```json
{
  "filename": "user-preferences"
}
```

Returns the entry content, or a "not found" message.

## Consolidation

Memory consolidation runs as scheduled tasks (seeded on first migration to V34):

### Nightly (3 AM)

Reviews the day's sessions, extracts durable information, and either appends to existing long-term entries, creates new topic entries, or skips if already captured. Never duplicates existing information.

### Weekly (4 AM Sunday)

Reviews all long-term entries for duplicates, outdated info, entries to merge, and verbose entries to tighten. Also prunes the embedding cache (entries not accessed in 30 days).

## IDENTITY.md

The personality file is the only memory that stays as a filesystem file — it's loaded as the first part of the system prompt. The agent can modify its own personality by writing to `IDENTITY.md`. Changes persist across sessions.

Created by `borg init` based on the chosen personality style.

## HEARTBEAT.md

Optional checklist at `~/.borg/HEARTBEAT.md`. When present, it is injected into heartbeat agent turns. See [Heartbeat](heartbeat.md).

## Session persistence

Conversations are saved to the SQLite database (`borg.db`). Each message is written immediately (crash recovery). Sessions track full message history, token usage, model, and auto-generated title.

## Migration from filesystem memory

V34 migration automatically imports existing `~/.borg/MEMORY.md` and `~/.borg/memory/*.md` files into the `memory_entries` table. Original files are renamed to `.bak`. The `memory/daily/` directory files are also imported.

## Tips

- Keep the INDEX entry as a concise index of what the agent knows
- The agent will naturally organize topic entries as you interact with it
- With embeddings enabled, the most relevant memories are loaded regardless of recency
- Use project-scoped memory to keep project-specific context separate
