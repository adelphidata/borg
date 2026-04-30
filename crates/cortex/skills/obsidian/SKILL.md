---
name: obsidian
description: "Work with Borg's markdown knowledge files as an Obsidian-friendly workspace"
requires:
  bins: []
---

# Obsidian

Use this skill when the user wants Borg to work with an Obsidian-style markdown workspace.

## Scope

Treat `~/.borg/memory/` as Borg's primary Obsidian-facing markdown surface.

- `~/.borg/memory/MEMORY.md` is the top-level curated index
- other topic notes live alongside it
- scoped notes live under `~/.borg/memory/scopes/<scope>/`

This skill is optional. Most Borg users will keep the default memory flow without using Obsidian at all.

## What Borg Owns

SQLite remains the source of truth for long-term memory entries accessed through `write_memory` and `read_memory`.

Working memory remains session-local and ephemeral. Do not invent a markdown-backed short-term memory system. Only write markdown when the user asks to file notes, compile knowledge, or maintain an Obsidian-friendly document.

## File Handling Rules

- Prefer existing `.borg` markdown surfaces instead of creating a new `vault/` directory
- Do not treat all of `~/.borg` as an Obsidian vault
- Avoid touching runtime internals such as `borg.db`, `*.sock`, logs, caches, secrets, or other non-markdown files
- Only access or update other markdown-like files in `~/.borg` when they are explicitly referenced or clearly part of the user's knowledge workflow

## Editing Style

Prefer stable filenames, YAML frontmatter when it adds value, and Obsidian-style `[[wikilinks]]` between related notes.

When updating a mixed-ownership note, preserve user-authored content outside managed regions. Use deterministic managed markers:

```md
<!-- borg:managed:start name=summary -->
... Borg-managed content ...
<!-- borg:managed:end name=summary -->
```

Borg may fully rewrite content inside managed regions, but should preserve content outside them.

If a file is clearly agent-owned, say so in the file's frontmatter or top-level note content and keep the structure deterministic.

## Search and Navigation

Use shell tools to inspect the workspace:

```bash
MEMORY_DIR="$HOME/.borg/memory"

rg --files "$MEMORY_DIR"
rg -n "query" "$MEMORY_DIR"
```

When useful, follow backlinks and wikilinks:

```bash
rg -n '\[\[[^]]+\]\]' "$MEMORY_DIR"
rg -n '\[\[target-note\]\]' "$MEMORY_DIR"
```

## Relationship To Borg Memory

- Use `memory_search` when the user is asking about prior knowledge or stored context
- Remember that `memory_search` already covers both SQLite long-term memory and markdown knowledge files
- Use `write_memory` for DB-backed durable memory entries
- Use markdown notes under `~/.borg/memory/` for curated knowledge artifacts intended to be browsed in Obsidian
