# Patch DSL

Borg uses a custom patch DSL for creating, modifying, and deleting files. The agent uses this via built-in tools: `apply_patch`, `apply_skill_patch`, and `create_channel`.

## Format

A patch is wrapped in `*** Begin Patch` / `*** End Patch` markers and contains one or more file operations. Every content line must have a prefix (`+` for added content, ` ` for context, `-` for removed lines):

```
*** Begin Patch
*** Add File: path/to/file.txt
+file contents here
*** End Patch
```

## Operations

### Add File

Creates a new file with the given contents:

```
*** Begin Patch
*** Add File: src/greeting.py
+import json, sys
+args = json.loads(sys.stdin.read())
+print(f"Hello, {args['name']}!")
*** Add File: src/config.toml
+name = "greeting"
+description = "A simple greeting script"
*** End Patch
```

Multiple files can be added in a single patch.

### Update File

Modifies an existing file using unified diff hunks:

```
*** Begin Patch
*** Update File: my-tool/main.py
@@
 import json, sys
+import os
 args = json.loads(sys.stdin.read())
-print(f"Hello, {args['name']}!")
+name = args.get('name', os.getenv('USER', 'world'))
+print(f"Hello, {name}!")
*** End Patch
```

Hunk format follows unified diff conventions:
- `@@` — hunk header (context hint is optional)
- Lines starting with ` ` (space) are context (must match existing content)
- Lines starting with `-` are removed
- Lines starting with `+` are added

Multiple hunks can be applied to the same file in a single update.

### Move File

Renames a file as part of an update. The `*** Move to:` line follows immediately after `*** Update File:`:

```
*** Begin Patch
*** Update File: old-name.py
*** Move to: new-name.py
@@
 context line
-old line
+new line
*** End Patch
```

### Delete File

Removes a file:

```
*** Begin Patch
*** Delete File: my-tool/old-script.py
*** End Patch
```

## Mixed operations

A single patch can combine add, update, move, and delete operations:

```
*** Begin Patch
*** Add File: src/new_module.py
+import json, sys
+print("new module")
*** Update File: src/existing.py
@@
 import json, sys
-print("old version")
+print("new version")
*** Delete File: src/deprecated.py
*** End Patch
```

## Base directories

Each patch tool operates on a different base directory:

| Tool | Base Directory | Description |
|------|---------------|-------------|
| `apply_patch` | `$CWD` (current working directory) | General-purpose file operations |
| `apply_skill_patch` | `~/.borg/skills/` | Create/modify user skills |
| `create_channel` | `~/.borg/channels/` | Create/modify channel integrations |

File paths in the patch are relative to the tool's base directory.

## Error handling

The parser validates:
- Patch markers are present and properly formatted
- Context lines match the existing file content
- Files to update exist; files to add don't already exist (unless overwriting)
- Heredoc wrapping is tolerated and automatically stripped

If any operation fails, the error is returned to the agent with details about what went wrong.

Recovery-oriented behaviors:

- **Already applied → success no-op.** If every hunk of an update op finds its
  *post-patch* text already in place, the op succeeds without writing and is
  reported as `= path (already applied — no changes made)`. Re-sending an
  applied patch therefore does not error. A *mix* of already-applied and
  pending hunks is a hard error (nothing is written) that names the applied
  hunks — re-read the file and patch only what's missing.
- **Ambiguous context → candidate list.** When search text matches multiple
  locations, the error lists up to 5 candidates as `L<line>: <snippet>` and
  suggests anchoring the hunk with `@@ <enclosing symbol>`.
- **No match → nearest-context hint.** The error shows the closest matching
  file lines with line numbers so the retry can be corrected without a
  re-read.
