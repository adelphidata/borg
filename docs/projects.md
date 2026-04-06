# Projects

Projects group related workflows into logical workstreams. A single initiative (e.g., "Ship v2 Release") can have multiple workflows, each queryable by `project_id`.

## Schema

```sql
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active',  -- active | archived
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
```

## CLI

```sh
borg projects                              # list all projects
borg projects list                         # list all projects
borg projects list --status active         # filter by status
borg projects create --name "My Project"   # create a project
borg projects create --name "My Project" --description "Details here"
borg projects get <id>                     # show project + workflows
borg projects update <id> --name "New Name"
borg projects update <id> --status archived
borg projects archive <id>                 # shorthand for status=archived
borg projects delete <id>                  # permanently remove
```

## TUI

`/projects` — displays all projects with status, description, and workflow counts.

## Agent tool

The `projects` tool lets the agent manage projects autonomously:

```json
{"action": "create", "name": "Auth Rewrite", "description": "Migrate to OAuth2"}
{"action": "list"}
{"action": "list", "status": "active"}
{"action": "get", "id": "abc12345"}
{"action": "update", "id": "abc12345", "name": "New Name", "status": "archived"}
{"action": "archive", "id": "abc12345"}
{"action": "delete", "id": "abc12345"}
```

## Linking workflows to projects

When creating a workflow via the `schedule` tool, pass `project_id` to associate it:

```json
{
  "action": "create",
  "type": "workflow",
  "name": "Build & Test",
  "goal": "Run CI pipeline",
  "project_id": "abc12345",
  "steps": [...]
}
```

## Key files

| File | Purpose |
|------|---------|
| `crates/core/src/db/workflow.rs` | Project + workflow DB operations |
| `crates/core/src/db/models.rs` | `ProjectRow` struct |
| `crates/core/src/tool_handlers/projects.rs` | `projects` tool handler |
| `crates/core/src/tool_definitions.rs` | Tool JSON schema |
| `crates/cli/src/main.rs` | `borg projects` CLI subcommand |
| `crates/cli/src/tui/commands.rs` | `/projects` TUI command |
