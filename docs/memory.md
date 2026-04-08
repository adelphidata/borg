# Memory

Borg has a persistent memory system that carries context across sessions. The agent can read and write memory files, and relevant memories are automatically loaded into the system prompt each turn.

## Memory files

All memory lives in `~/.borg/` and project directories:

| File | Purpose | Loaded |
|------|---------|--------|
| `IDENTITY.md` | Personality and behavioral instructions | Always (as system prompt prefix) |
| `MEMORY.md` | Memory index -- high-level notes | Always (first in memory context) |
| `memory/*.md` | Topic-specific memories | By relevance (semantic search) or recency |
| `$CWD/.borg/memory/*.md` | Per-project local memories | In addition to global memory |
| `memory/scopes/{scope}/*.md` | Scoped memories (per-binding or per-agent) | When scope is active |
| `HEARTBEAT.md` | Checklist for heartbeat agent | During heartbeat turns only |

## How memory loading works

Each turn, the agent builds a memory context:

1. **MEMORY.md** is loaded first (always included if it fits the budget)
2. **memory/*.md** files are ranked by semantic similarity to the user's query, blended with recency
3. Files are included one by one until the token budget is exhausted
4. **Local memory** (`$CWD/.borg/memory/*.md`) is loaded in addition to global memory when present
5. **Scoped memory** is loaded when `memory.memory_scope` or a gateway binding `memory_scope` is set

### Semantic search

Embeddings are generated on `write_memory` and stored in SQLite. Memory files are ranked by cosine similarity to the user's query, blended with recency. The system uses hybrid search combining BM25 keyword matching with vector similarity.

Auto-detects embedding provider from API keys (OpenAI -> OpenRouter -> Gemini). Silently falls back to recency-only ranking when no embedding provider is available (e.g., Anthropic-only users).

Configure via `borg settings set`:

```sh
borg settings set memory.embeddings.enabled true
borg settings set memory.embeddings.recency_weight 0.2    # 0.0=pure similarity, 1.0=pure recency
borg settings set memory.embeddings.bm25_weight 0.3       # BM25 keyword weight
borg settings set memory.embeddings.vector_weight 0.7     # vector similarity weight
```

Token estimation uses tiktoken-rs (cl100k_base BPE tokenizer).

### Configuration

```sh
borg settings set memory.max_context_tokens 8000
borg settings set memory.memory_scope "my-project"   # optional: namespace for scoped memory
```

## Writing memory

The agent uses the `write_memory` tool:

```json
{
  "filename": "user_preferences.md",
  "content": "# User Preferences\n\n- Prefers concise answers\n- Timezone: PST",
  "append": false,
  "scope": "global"
}
```

- `filename`: target file (`IDENTITY.md`, `MEMORY.md`, or any name for a topic file)
- `content`: text to write
- `append`: if `true`, appends to existing content instead of overwriting
- `scope`: `"global"` (default, writes to `~/.borg/memory/`) or `"local"` (writes to `$CWD/.borg/memory/`)

Special filenames:
- `IDENTITY.md` -- writes to `~/.borg/IDENTITY.md` (personality)
- `MEMORY.md` -- writes to `~/.borg/MEMORY.md` (index)
- Anything else -- writes to `~/.borg/memory/<filename>` (or `$CWD/.borg/memory/<filename>` if scope is local)

## Reading memory

The agent uses the `read_memory` tool:

```json
{
  "filename": "user_preferences.md"
}
```

Returns the file contents, or a "not found" message if the file doesn't exist.

## Security

Memory filenames are validated to prevent path traversal:
- No `..` sequences allowed
- No `/` or `\` separators allowed
- Empty filenames are rejected

Secrets in tool output are automatically redacted when `security.secret_detection` is enabled (default: true).

## IDENTITY.md

The personality file is special -- it's loaded as the first part of the system prompt (before memory context). The agent can modify its own personality by writing to `IDENTITY.md`. Changes persist across sessions.

The default `IDENTITY.md` is created by `borg init` based on the chosen personality style (Professional, Casual, Snarky, Nurturing, or Minimal).

## HEARTBEAT.md

Optional checklist at `~/.borg/HEARTBEAT.md`. When present, it is injected into heartbeat agent turns so the agent can check email, calendar, or other periodic tasks. See [Heartbeat](heartbeat.md).

## Session persistence

Conversations are automatically saved to the SQLite database (`borg.db`). Each message is written immediately when added to history, enabling crash recovery. Sessions track:

- Full message history (with timestamps)
- Token usage
- Model used
- Auto-generated title

The most recent session can be automatically loaded on startup.

## Conversation compaction

When conversation history exceeds the token budget (`conversation.max_history_tokens`, default 32000), the agent can compact history by summarizing older messages while preserving recent context. This can also be triggered manually via the `/compact` slash command.

## Atomic rollback

The `/undo` command rolls back the last agent turn (both the assistant response and the preceding user message), allowing you to retry or rephrase.

## Memory cleanup

The `/memory cleanup` command helps manage memory files by identifying and removing stale or redundant entries.

## Tips

- Keep `MEMORY.md` as a concise index of what the agent knows
- Use topic-specific files (`memory/project-x.md`, `memory/meeting-notes.md`) for detailed context
- The agent will naturally learn to use memory as you interact with it
- With embeddings enabled, the most relevant memories are loaded regardless of recency
- Use `scope: "local"` to keep project-specific memories separate from global ones
