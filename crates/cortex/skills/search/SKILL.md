---
name: search
description: "Find files and search content via ripgrep and fd"
triggers:
  - "find this in the codebase"
  - "search the repo"
  - "ripgrep"
  - "find matching files"
requires:
  bins: ["rg"]
---

# Search

`rg` for content, `fd` for filenames. Both respect `.gitignore` —
`rg --no-ignore` / `fd -I` to include ignored, `-H` for hidden.
`find . -name "…"` as fallback if `fd` missing. `rg --type-list` for
built-in filters. Exclude with `-g '!vendor'`.

## rg

```bash
rg "pattern" [path]
rg "fn main" --type rust
rg "TODO|FIXME" --type-add 'code:*.{rs,py,ts}' -tcode
rg "pattern" -l         # file list only
rg "pattern" -c         # match counts
rg "pattern" -C 3       # context lines
rg "https?://\S+" -o    # extract matches
rg "old" -r "new"       # preview replacement (no write)
```

## fd

```bash
fd "\.rs$"
fd "config" --type f
fd "test"   --type d
fd -e toml
fd -H ".env"
```

## Combine

```bash
fd -e rs | xargs rg "unwrap()"
rg -l "TODO" | xargs wc -l
fd -e json --exec jq '.name' {}
```
