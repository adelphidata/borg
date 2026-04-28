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
- Active project: tracks project context from the projects tool
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

## IDENTITY.md

`IDENTITY.md` is not part of the general memory stores. It is loaded at the front of the system prompt and defines the assistant’s persistent personality and behavior.

## HEARTBEAT.md

`HEARTBEAT.md` is an optional checklist used only during heartbeat turns.

## Session persistence

Conversations are saved in `borg.db` immediately for crash recovery. Session transcripts are searchable through `memory_search` when `sources=["messages"]` is requested or when active memory injection searches both memory and messages.

## Tips

- Keep `INDEX` concise and high-signal
- Keep `MEMORY.md` as a curated map of important knowledge files
- Use scoped memory for project- or channel-specific context
- Prefer topic-sized long-term entries over one huge catch-all note
