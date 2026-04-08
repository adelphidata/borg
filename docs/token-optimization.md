# Token Optimization

Borg includes several configurable token optimizations that reduce inference costs
by 30-40% on typical sessions. Each optimization has an independent config flag
for instant enable/disable.

## Why It Matters

Every LLM turn sends the full system prompt, tool schemas, and conversation history.
On a 20-turn coding session, cumulative input tokens can reach 40K-60K per turn.
These optimizations reduce that to ~25K-40K without sacrificing functionality.

## Optimization Inventory

| Optimization | Config Key | Default | Est. Savings/Turn | Risk |
|---|---|---|---|---|
| Conditional tool inclusion | `tools.conditional_loading` | `true` | 500-1500 tokens | Low-Medium |
| Tool schema compression | `tools.compact_schemas` | `true` | 200-600 tokens | Low-Medium |
| Tiered history degradation | `conversation.age_based_degradation` | `true` | 2K-8K tokens | Medium |
| Chunk-level memory selection | `memory.chunk_level_selection` | `false` | 1K-4K tokens | Medium-High |
| Adaptive cache TTL | `llm.cache.ttl` | `"auto"` | Cache hit improvement | Low |
| Conditional prompt sections | (no flag, profile-based) | always | 100-300 tokens | Low |

## Config Reference

Configure via `borg settings set`:

```sh
# Only send tool schemas relevant to the current message.
# Core tools (memory, fs, runtime, discovery) always included.
# Conditional groups (web, browser, schedule, media, integration, agents)
# included when keywords match or tools were recently used.
borg settings set tools.conditional_loading true

# Strip redundant metadata from tool schemas (remove defaults,
# shorten enum descriptions). Saves ~200-600 tokens.
borg settings set tools.compact_schemas true

# Progressively degrade old tool results:
# - Last 12 messages: full fidelity
# - Messages 13-24: truncate large results to head+tail
# - Older than 24: replace with one-line status summary
borg settings set conversation.age_based_degradation true

# Load memory at section granularity instead of file granularity.
# Requires embeddings enabled. Default: false (opt-in).
borg settings set memory.chunk_level_selection false

# Adaptive cache TTL. "auto" uses 5m for REPL, 1h for gateway/scheduled.
borg settings set llm.cache.ttl auto
```

## How Each Optimization Works

### Conditional Tool Inclusion

**Mechanism:** Before each LLM call, the user message is scanned for keyword hints
associated with each tool group. Groups like `Web`, `Ui`, `Scheduling`, `Media`,
`Integration`, and `Agents` are only included when their keywords match or when
a tool from that group was used in a recent turn.

**Fallback safety:** If the LLM returns a tool call for an excluded tool, the turn
is re-run with all tools included. This should be extremely rare.

**Keywords per group:**
- Web: search, fetch, url, website, http, link, web, scrape
- UI: browser, screenshot, click, navigate, webpage, dom, scrape, open page
- Scheduling: schedule, cron, remind, recurring, every day, weekly, timer, alarm
- Media: image, generate image, picture, draw, photo, illustration
- Integration: email, gmail, calendar, notion, linear, slack, discord
- Agents: agent, spawn, delegate, parallel, background task, sub-agent, subagent

### Tool Schema Compression

**Mechanism:** After building tool definitions, a post-processor removes redundant
metadata from JSON schemas:
- Removes `"default"` keys (LLM infers defaults from descriptions)
- For properties with `"enum"` constraints, removes parenthetical enum listings
  from descriptions (the constraint already communicates valid values)

### Tiered History Degradation

**Mechanism:** Before each LLM call, old tool results are progressively degraded:
- **Tier 1** (last 12 messages): full fidelity
- **Tier 2** (messages 13-24): tool results over 200 tokens → first 3 lines +
  "[N lines omitted]" + last 2 lines
- **Tier 3** (older than 24): tool results over 50 tokens → `[tool result {id} — ok]`

Runs before the existing compaction cascade (share limit → tool result compaction → LLM summarization), so it reduces the load on those more expensive operations.

### Chunk-Level Memory Selection

**Mechanism:** When enabled, memory files are split into sections (by `## ` headers)
and ranked individually by embedding similarity. Only the most relevant sections
are packed into the token budget, with `<!-- from: filename / section -->` provenance
comments.

**Currently opt-in** because it requires embeddings and risks missing cross-section
context. Enable only if you have large memory files where most content is irrelevant
to typical queries.

### Adaptive Cache TTL

**Mechanism:** Anthropic prompt caching supports 5-minute and 1-hour TTLs. The
`"auto"` setting uses 5m for interactive REPL sessions (rapid back-and-forth) and
1h for gateway/scheduled sessions where inter-turn latency exceeds 5 minutes.

1-hour TTL has a slightly higher cache write cost but dramatically reduces cache
misses for sessions with longer pauses between turns.

### Conditional Prompt Sections

**Mechanism:** The `<coding_instructions>` block (~100 tokens) is only included when
the active tool profile includes Filesystem or Runtime groups. Messaging-only or
minimal profiles skip it entirely.

## Monitoring Checklist

After enabling optimizations, watch for these signals:

| Signal | Optimization | Action |
|---|---|---|
| Tool call errors (wrong params) | Schema compression | Set `compact_schemas = false` |
| Fallback re-runs >5% of turns | Conditional tools | Widen keyword hints or set `conditional_loading = false` |
| LLM re-requests degraded info | History degradation | Increase tier windows or set `age_based_degradation = false` |
| Memory recall accuracy drops | Chunk-level memory | Set `chunk_level_selection = false` |
| Cache hit rate drops | Cache TTL | Set explicit `ttl = "1h"` or `ttl = "5m"` |

## Revert Procedures

Each optimization can be instantly disabled via `borg settings set` and restarting
the session. No data migration or cleanup is needed — all changes are per-turn
and stateless.

```sh
# Emergency: disable all optimizations
borg settings set tools.conditional_loading false
borg settings set tools.compact_schemas false
borg settings set conversation.age_based_degradation false
borg settings set memory.chunk_level_selection false
borg settings set llm.cache.ttl 5m
```

## Changelog

- **2026-04-07**: Initial implementation. All 6 optimizations added with config flags,
  test coverage, and documentation. Chunk-level memory defaults to off (opt-in).
