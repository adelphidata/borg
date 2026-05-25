---
name: notes
description: "Create, search, and organize Markdown notes"
requires:
  bins: []
---

# Notes

Markdown notes / Obsidian vaults via shell. Default vault `~/notes/`; ask if
unclear. Prefer `rg`/`fd` if installed.

## Create

```bash
cat > ~/notes/2026-03-14-meeting.md << 'EOF'
---
title: Meeting Notes
date: 2026-03-14
tags: [meeting, project]
---

# Meeting Notes

## Action Items
- [ ]
EOF
```

Daily note:

```bash
d=$(date +%Y-%m-%d); cat > ~/notes/daily/${d}.md <<EOF
---
date: ${d}
---

# ${d}

## Tasks
- [ ]
EOF
```

## Search

```bash
find ~/notes -name "*.md" | grep -i "meeting"          # by name
grep -rln "search term" ~/notes/ --include="*.md" -C 2 # by content
```

## Obsidian wikilinks

```bash
grep -rl "\[\[target-note\]\]" ~/notes/ --include="*.md"     # backlinks
grep -oP '\[\[([^\]]+)\]\]' ~/notes/my-note.md                # outgoing
# orphans:
for f in ~/notes/*.md; do n=$(basename "$f" .md); \
  [ "$(grep -rl "\[\[$n\]\]" ~/notes/ --include='*.md' | wc -l)" -eq 0 ] && echo "$n"; done
```

## Tags

```bash
grep -rl "tags:.*project" ~/notes/ --include="*.md"               # frontmatter
grep -rohP '#[a-zA-Z][\w/-]+' ~/notes/ --include="*.md" | sort -u # inline #tags
```
