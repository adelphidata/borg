# Memory System Comparison: Borg vs OpenClaw vs Hermes

## Context

Research comparison of three agent memory systems to understand strengths, weaknesses, and improvement opportunities. OpenClaw's memory has been reported as less than ideal — this analysis identifies concrete gaps and what to borrow from the other two systems.

---

## Architecture Summary

| Dimension | **Borg** (Rust) | **OpenClaw** (TypeScript) | **Hermes** (Python) |
|-----------|-----------------|--------------------------|---------------------|
| Storage | Markdown files + SQLite (embeddings, chunks, FTS5) | Markdown files + SQLite (embeddings, chunks, FTS5) | Markdown files (MEMORY.md, USER.md) + SQLite (sessions, FTS5) |
| Search | Hybrid: vector (70%) + BM25 (30%) + MMR diversity | Hybrid: vector + BM25 + MMR + temporal decay | FTS5 keyword only (built-in); HRR vectors via plugin |
| Scoping | Global / local (per-CWD) / extra paths / scoped subdirs | Per-agent workspace | Two stores: MEMORY.md (agent notes) + USER.md (user profile) |
| Capacity | Token-budgeted (default 8K tokens), unlimited files | Token-budgeted (model-aware), unlimited files | Hard char limits: 2,200 chars (memory) + 1,375 chars (user) |
| Context loading | Ranked by embedding similarity, fallback to recency | Ranked by embedding similarity, fallback to recency | Frozen snapshot at session start, never updated mid-session |
| Pre-compaction | LLM extracts durable facts -> daily/{date}.md | LLM flush with dedup (content hash gating) | `on_pre_compress` hook notifies providers; structured LLM summary |
| Embedding providers | OpenAI -> OpenRouter -> Gemini (auto-detect) | OpenAI, Gemini, Voyage, Mistral, Ollama, local GGUF | None built-in; Holographic plugin uses deterministic SHA-256 phase vectors |
| Write mechanism | `write_memory` tool (overwrite/append, scope param) | Agent writes markdown files directly | `memory` tool with add/replace/remove actions, substring matching |
| File watching | `notify` crate, 1.5s debounce, auto re-embed | Chokidar, 1.5s debounce, auto re-index | None (files read at session start) |

---

## Borg

### Pros
1. **Strongest hybrid search** -- weighted vector (0.7) + BM25 (0.3) blend with MMR diversity re-ranking and per-term fallback. Three layers of search ensure something always matches.
2. **Incremental chunk-level indexing** -- file watcher detects changes, only re-embeds modified chunks (hash comparison). Minimal API cost.
3. **Rich scoping** -- global, local (per-project), scoped subdirectories, extra user-configured paths. Most flexible of the three.
4. **Token-aware progressive loading** -- ranked files loaded first, unranked by recency as fallback, all within configurable token budget.
5. **Pre-compaction flush with signal gate** -- LLM extracts durable facts; "SKIP" gate prevents noise from being written to daily logs.
6. **Compiled performance** -- Rust + SQLite FTS5 = sub-millisecond keyword search, parallel ranking across scopes.

### Cons
1. **No temporal decay** -- unlike OpenClaw, old memories aren't downweighted by age. Stale facts compete equally with fresh ones.
2. ~~No prompt injection scanning~~ -- **Fixed**: Borg now scans for prompt override, exfiltration, and invisible Unicode patterns on all memory writes.
3. **Approximate token estimation** -- rough calculation, not exact. XML wrapping overhead not fully accounted for.
4. **Configuration complexity** -- 11+ tuning knobs (vector_weight, bm25_weight, mmr_lambda, chunk_size, overlap, etc.) with no built-in guidance.
5. **No memory versioning** -- no history of what changed when. Overwrites are destructive.
6. **Unbounded chunk accumulation** -- old chunks from deleted/modified files can linger without cleanup.

---

## OpenClaw

### Pros
1. **Temporal decay** -- explicit half-life (default 30 days) downweights old daily notes. Evergreen files (MEMORY.md) exempt. Most sophisticated recency handling.
2. **Pluggable backends** -- builtin SQLite, QMD (local sidecar with reranking/query expansion), Honcho (cross-session AI memory). Most extensible architecture.
3. **Broadest embedding provider support** -- OpenAI, Gemini, Voyage, Mistral, Ollama, local GGUF fallback. Works offline.
4. **Model-aware context windows** -- dynamically reads model metadata for budget (200K default, 1M for Opus/Sonnet 4). Adapts to model.
5. **Content hash dedup on flush** -- SHA-256 of recent messages prevents duplicate pre-compaction writes. Smart gate.
6. **CJK-aware MMR** -- Jaccard tokenization handles Chinese/Japanese/Korean correctly for diversity.

### Cons
1. **Per-agent isolation** -- memory doesn't naturally cross between agents. No shared knowledge base.
2. **No structured memory writes** -- agent writes raw markdown files. No validation, no entry management, no character limits. Memory can bloat.
3. **No injection scanning** -- unlike Hermes, no threat pattern checks on memory content.
4. **Embedding cache grows unbounded** -- no pruning logic for stale embeddings in SQLite.
5. **QMD adds operational complexity** -- second backend means two failure modes, two config surfaces, two debugging paths.
6. **SQLite readonly recovery issues** -- readonly errors require connection reset and full re-sync. Fragile under load.
7. **No atomic file writes** -- unlike Hermes (temp file + os.replace), concurrent readers can see partial writes.
8. **Session memory experimental** -- cross-session indexing exists but is behind experimental flags.

---

## Hermes

### Pros
1. **Frozen snapshot pattern** -- system prompt built once at session start and cached. Guarantees LLM prefix cache hits across entire session. Most cache-friendly design.
2. **Prompt injection defense** -- scans all memory writes for injection patterns, exfiltration attempts, invisible Unicode. Only system that does this.
3. **Atomic file writes** -- temp file + `os.replace()` prevents corruption from concurrent access.
4. **Bounded capacity** -- hard character limits (2.2K + 1.4K) keep system prompt predictable and small. No surprise token bloat.
5. **Pluggable provider architecture** -- MemoryProvider ABC with lifecycle hooks (turn start, pre-compress, session end, delegation). Clean extensibility.
6. **Holographic Reduced Representations** -- deterministic, reproducible vectors (no ML infrastructure needed). Compositional algebra: bind/unbind/bundle operations enable algebraic reasoning about facts. Unique capability.
7. **Trust scoring** -- facts have confidence scores that adjust via feedback. Memory quality improves over time.
8. **Subagent isolation** -- subagents created with `skip_memory=True`, parent observes via hook. Prevents memory corruption from transient sessions.

### Cons
1. **No semantic search in built-in memory** -- substring matching only for replace/remove. No embeddings, no vector search in the core system.
2. **Tiny capacity** -- ~8-15 entries max in built-in memory. Severely limits what the agent can remember without plugins.
3. **Stale within session** -- frozen snapshot means writes during a session don't appear until next session. Agent can't use what it just wrote.
4. **FTS5 only for session search** -- keyword matching across sessions, no vector similarity. Misses semantic connections.
5. **Bag-of-words HRR limitation** -- holographic vectors capture token presence but not word order. "dog bites man" = "man bites dog".
6. **No file watching** -- memory files read only at session start. External edits invisible until restart.
7. **No project scoping** -- no per-project memory isolation. Everything is global to the agent.
8. **Auxiliary model dependency** -- session search summarization requires a cheap model (Gemini Flash). Falls back to None if unavailable.

---

## Head-to-Head: Key Dimensions

### 1. Search Quality
**Winner: Borg** > OpenClaw > Hermes

Borg's three-tier search (hybrid blend -> MMR diversity -> per-term fallback) is the most robust. OpenClaw matches on hybrid + MMR but adds temporal decay (an advantage for time-sensitive content). Hermes has no semantic search in its core -- only keyword FTS5 and the optional Holographic plugin.

### 2. Context Efficiency
**Winner: Hermes** > Borg > OpenClaw

Hermes' frozen snapshot + hard char limits = perfect prefix cache hits and predictable token usage. Borg's progressive token-budgeted loading is good but approximate. OpenClaw's model-aware budgeting is sophisticated but relies on token estimates.

### 3. Extensibility
**Winner: OpenClaw** > Hermes > Borg

OpenClaw has pluggable backends (builtin, QMD, Honcho), broadest embedding providers, and the most flexible configuration. Hermes has a clean MemoryProvider ABC. Borg is monolithic -- powerful but all-in-one.

### 4. Safety
**Winner: Hermes** > Borg > OpenClaw

Hermes scans for prompt injection, validates content, uses atomic writes. Borg has blocked-path security and symlink validation but no content scanning. OpenClaw has neither injection scanning nor atomic writes.

### 5. Freshness / Recency
**Winner: OpenClaw** > Borg > Hermes

OpenClaw's temporal decay explicitly models information aging. Borg uses mtime-based recency as a loading fallback. Hermes' frozen snapshot is the worst -- writes during a session are invisible until next session.

### 6. Cross-Session Continuity
**Winner: Borg** > OpenClaw > Hermes

Borg's pre-compaction flush extracts durable facts automatically with an LLM signal gate, writes to searchable daily logs. OpenClaw has similar flush mechanics with content hash dedup. Hermes relies on `on_session_end` hooks and plugin-specific persistence.

---

## Improvement Ideas for OpenClaw

Based on the comparison, concrete areas where OpenClaw's memory could be improved by borrowing from the other two systems:

### From Hermes
1. **Add prompt injection scanning** -- Scan memory writes for injection patterns, exfiltration attempts, and invisible Unicode before persisting. Hermes' `_MEMORY_THREAT_PATTERNS` is a good starting point.
2. **Atomic file writes** -- Use temp file + rename instead of direct writes to prevent corruption under concurrent access.
3. **Bounded memory stores** -- Consider optional character/token limits per memory file to prevent unbounded growth and token budget surprises.
4. **Trust/confidence scoring** -- Track which memory entries are actually useful (feedback loop). Age alone isn't enough signal.
5. **Frozen snapshot option** -- For latency-sensitive deployments, offer a mode where memory is loaded once at session start for prefix cache efficiency.

### From Borg
1. **Per-term fallback search** -- When hybrid search returns 0 results, split query into individual terms and search each. Simple but effective catch-all.
2. **Pre-compaction signal gate** -- The "SKIP" pattern (LLM returns SKIP if nothing worth saving) prevents noise accumulation in daily logs. OpenClaw's content hash dedup is good but doesn't filter quality.
3. **Multi-scope memory** -- Add project-scoped memory (per-CWD) alongside per-agent memory. Borg's global/local/scoped model is more flexible.
4. **Chunk-level change detection** -- Only re-embed changed chunks (by hash), not entire files. Reduces embedding API costs.

### Standalone Improvements
1. **Embedding cache pruning** -- Add TTL or LRU eviction to the embedding cache table.
2. **Memory file versioning** -- Track changes to memory files (even just last-N versions) so accidental overwrites are recoverable.
3. **Cross-agent memory sharing** -- Allow agents to opt into shared memory pools for team/org knowledge.
4. **Adaptive chunking** -- Instead of fixed 400-token chunks, use markdown-aware chunking (by headers, paragraphs) for better semantic boundaries.
5. **Search quality metrics** -- Log which search results the agent actually uses, creating a feedback signal for tuning weights.
