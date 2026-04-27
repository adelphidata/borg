---
name: notes
description: "Create, search, and organize Markdown notes"
requires:
  bins: []
---

# Notes Skill

Manage markdown notes and Obsidian vaults using standard shell tools.

## Create Notes

```bash
# Create a new note with frontmatter
cat > ~/notes/2026-03-14-meeting.md << 'EOF'
---
title: Meeting Notes
date: 2026-03-14
tags: [meeting, project]
---

# Meeting Notes

## Attendees
- ...

## Action Items
- [ ] ...
EOF
```

## Search Notes

```bash
# Find notes by title or filename
find ~/notes -name "*.md" | grep -i "meeting"

# Search note contents
grep -rl "search term" ~/notes/ --include="*.md"

# Search with context
grep -rn "search term" ~/notes/ --include="*.md" -C 2
```

## Obsidian Wiki Links

```bash
# Find all notes linking to a specific note
grep -rl "\[\[target-note\]\]" ~/notes/ --include="*.md"

# List all wiki links in a note
grep -oP '\[\[([^\]]+)\]\]' ~/notes/my-note.md

# Find orphaned notes (no incoming links)
for f in ~/notes/*.md; do
  name=$(basename "$f" .md)
  count=$(grep -rl "\[\[$name\]\]" ~/notes/ --include="*.md" | wc -l)
  [ "$count" -eq 0 ] && echo "Orphan: $name"
done
```

## Daily Notes

```bash
# Create today's daily note
date_str=$(date +%Y-%m-%d)
cat > ~/notes/daily/${date_str}.md << EOF
---
date: ${date_str}
---

# ${date_str}

## Tasks
- [ ]

## Notes

EOF
```

## Tags & Metadata

```bash
# Find notes with a specific tag
grep -rl "tags:.*project" ~/notes/ --include="*.md"

# List all tags across vault
grep -rohP '#[a-zA-Z][\w/-]+' ~/notes/ --include="*.md" | sort -u
```

## Notes

- Default vault path is `~/notes/`; adjust to match the user's actual vault location
- Obsidian uses `[[wikilinks]]` and YAML frontmatter for metadata
- Use `find` and `grep` as fallbacks when `rg` or `fd` are not available
- Always ask the user for their vault/notes directory if not obvious
