# Workflows

Durable multi-step task orchestration. Each step runs as an isolated agent turn with focused context, backed by SQLite for crash recovery.

## Why

Strong models (Opus 4.6) can manage complex multi-step tasks in a single conversation. Weaker models lose focus after 5-10 iterations. Workflows provide explicit structure: decompose a task into ordered steps, execute each independently, persist progress.

## How it works

1. The LLM decomposes a complex request into workflow steps via the `schedule` tool
2. The daemon picks up one step per 60-second tick
3. Each step gets a fresh agent turn with: workflow goal, prior step outputs, current instructions
4. On success, the output is saved and the next step begins
5. On failure, the step retries with backoff (previous error injected as context)
6. On crash, stale running steps are recovered on daemon restart

## Projects

Projects group related workflows. A single initiative (e.g., "Ship v2 release") can have multiple workflows, each queryable by `project_id`.

```
Project: "Ship v2 Release"
  Workflow 1: "Build & Test" (completed)
  Workflow 2: "Deploy to staging" (running)
  Workflow 3: "Production rollout" (pending)
```

## Schema

```sql
-- Groups related workflows
CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'active',  -- active | archived
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Durable multi-step task
CREATE TABLE workflows (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    goal TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | running | completed | failed | cancelled
    current_step INTEGER NOT NULL DEFAULT 0,
    session_id TEXT REFERENCES sessions(id),
    project_id TEXT REFERENCES projects(id),
    delivery_channel TEXT,
    delivery_target TEXT,
    ...
);

-- Individual step within a workflow
CREATE TABLE workflow_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workflow_id TEXT NOT NULL REFERENCES workflows(id) ON DELETE CASCADE,
    step_index INTEGER NOT NULL,
    title TEXT NOT NULL,
    instructions TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',  -- pending | running | completed | failed | skipped
    output TEXT,
    max_retries INTEGER NOT NULL DEFAULT 3,
    timeout_ms INTEGER NOT NULL DEFAULT 300000,
    ...
);
```

## Tool interface

Workflows use the existing `schedule` tool with `type: "workflow"`:

```json
{
  "action": "create",
  "type": "workflow",
  "name": "Deploy auth feature",
  "goal": "Build, test, and deploy the auth feature",
  "project_id": "proj-123",
  "steps": [
    {"title": "Run tests", "instructions": "Run cargo test. Report failures."},
    {"title": "Build release", "instructions": "Run cargo build --release."},
    {"title": "Deploy", "instructions": "Deploy to staging.", "max_retries": 3}
  ]
}
```

Actions: `create`, `list`, `get`, `cancel`

## Model-aware auto-enable

| Config value | Behavior |
|-------------|----------|
| `"auto"` (default) | All Claude models: off. Everything else: on. |
| `"on"` | Always enabled, all models see workflow option |
| `"off"` | Never enabled, workflow type hidden from tool schema |

Configure via `/settings` (Workflows toggle) or `config.toml`:

```toml
[workflow]
enabled = "auto"
```

## TUI management

`/schedule` opens a popup showing both scheduled tasks and workflows. For workflows:
- `c` — mark running workflow for cancellation
- `d` — mark for deletion
- `Enter` — apply pending changes

## Key files

| File | Purpose |
|------|---------|
| `crates/core/src/workflow/mod.rs` | Status constants, `workflows_active()` |
| `crates/core/src/workflow/engine.rs` | Step context template builder |
| `crates/core/src/workflow/tier.rs` | Model tiering heuristic |
| `crates/core/src/db/workflow.rs` | All DB operations (workflows, steps, projects) |
| `crates/core/src/tool_handlers/schedule.rs` | Tool handler dispatch |
| `crates/cli/src/service.rs` | Daemon step execution |
