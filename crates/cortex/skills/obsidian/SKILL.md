---
name: obsidian
description: "Work with Borg's markdown knowledge files as an Obsidian-friendly workspace"
requires:
  bins: []
---

# Obsidian

Optional workflow — most users won't use it. Treat `~/.borg/memory/` as the
Obsidian-facing surface: `MEMORY.md` is the curated top-level index; topic
notes alongside it; scoped notes under `scopes/<scope>/`.

SQLite remains the source of truth for `write_memory` / `read_memory`.
Working memory stays ephemeral — do **not** invent a markdown short-term
store. Only write markdown when the user explicitly asks to file notes or
maintain a doc.

## Rules

- Don't create a new `vault/` dir; reuse the existing `~/.borg/memory/` tree.
- Never touch `borg.db`, sockets, logs, caches, or secrets.
- For mixed-ownership notes, preserve user content outside managed regions:

  ```md
  <!-- borg:managed:start name=summary -->
  ... Borg-managed ...
  <!-- borg:managed:end name=summary -->
  ```

- Prefer stable filenames + `[[wikilinks]]`. Mark agent-owned files in
  frontmatter.

## Search

```bash
MEMORY_DIR="$HOME/.borg/memory"
rg --files "$MEMORY_DIR"
rg -n "query" "$MEMORY_DIR"
rg -n '\[\[[^]]+\]\]' "$MEMORY_DIR"            # all wikilinks
rg -n '\[\[target-note\]\]' "$MEMORY_DIR"      # backlinks
```

## Tool choice

- `memory_search` already covers both SQLite long-term memory and markdown
  knowledge files — use it first.
- `write_memory` → durable DB entries.
- Markdown under `~/.borg/memory/` → browsable knowledge artifacts.
