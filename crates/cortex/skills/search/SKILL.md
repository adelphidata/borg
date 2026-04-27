---
name: search
description: "Find files and search content via ripgrep and fd"
requires:
  bins: ["rg"]
---

# Search Skill

Use `rg` (ripgrep) for content search and `fd` for file name search.

## Content Search with rg

```bash
rg "pattern" .                          # search current directory recursively
rg "pattern" src/                       # search specific directory
rg "fn main" --type rust                # filter by file type
rg "TODO|FIXME" --type-add 'code:*.{rs,py,ts}'  # custom type
rg "pattern" -l                         # list matching files only
rg "pattern" -c                         # count matches per file
rg "pattern" -C 3                       # show 3 lines of context
```

## Regex Search

```bash
rg "fn \w+\(" --type rust               # find function definitions
rg "import .* from" --type ts           # find imports
rg "^## " --type md                     # find markdown headings
rg "https?://\S+" -o                    # extract URLs
```

## File Search with fd

```bash
fd "\.rs$"                              # find Rust files
fd "config" --type f                    # find files matching "config"
fd "test" --type d                      # find directories matching "test"
fd -e toml                              # find by extension
fd -H ".env"                            # include hidden files
```

## Combining Tools

```bash
fd -e rs | xargs rg "unwrap()"         # find unwrap in Rust files
rg -l "TODO" | xargs wc -l             # count lines in files with TODOs
fd -e json --exec jq '.name' {}        # extract field from all JSON files
```

## Replacement Preview

```bash
rg "old_name" -r "new_name"            # preview replacements (does not modify)
rg "old_name" -l                        # list files before bulk edit
```

## Notes

- `rg` respects `.gitignore` by default; use `--no-ignore` to override
- `fd` also respects `.gitignore`; use `-H` for hidden files, `-I` for ignored files
- If `fd` is not available, use `find . -name "pattern"` as a fallback
- Use `rg --type-list` to see all built-in file type filters
- Use `-g '!vendor'` to exclude specific directories
