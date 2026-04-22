# User Hooks

User hooks let you run shell commands on agent lifecycle events without rebuilding the binary. A single JSON file — `~/.borg/hooks.json` — registers commands that fire when sessions start/end, messages arrive, tool calls begin/end, or the turn stops.

The schema is compatible with Claude Code and [codex](https://github.com/openai/codex).

## Quick start

```json
{
  "hooks": {
    "PostToolUse": [
      { "matcher": "apply_patch|write_memory",
        "hooks": [{ "type": "command", "command": "git -C ~/.borg add -A && git -C ~/.borg commit -m auto" }] }
    ],
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "date >> ~/.borg/session.log" }] }
    ]
  }
}
```

Restart `borg`. Hooks are discovered at startup.

## Events

| JSON name | Fires… |
|---|---|
| `SessionStart` | Once when a new session begins. |
| `SessionEnd` | When a session ends. |
| `UserPromptSubmit` | Before the agent starts processing a user message. |
| `PreToolUse` | Before executing any tool call. Non-zero exit **skips** the tool call. |
| `PostToolUse` | After a tool call completes. Observer — exit code ignored. |
| `Stop` | After a full agent turn completes. |

Script hooks subscribe to these six events. The compiled-in `HookPoint` enum uses different names for two of them (`BeforeToolCall` = `PreToolUse`, `TurnComplete` = `Stop`) and adds three in-process-only variants (`BeforeLlmCall`, `AfterLlmResponse`, `OnError`) dispatched to compiled-in hooks (vitals, activity, bond, evolution) but not exposed to `hooks.json`.

## Handler config

```json
{ "type": "command", "command": "<shell string>", "timeout": <seconds> }
```

- `command` — passed to `sh -c`, so pipes, env vars, and redirects all work.
- `timeout` — seconds. Default `60`. Clamped to `[1, 600]`. Child is SIGKILL'd on expiry.
- `matcher` (on the group) — optional regex applied to the **tool name** (`run_shell`, `apply_patch`, etc.). Omit or use `"*"` to match all tools. Non-tool events ignore the matcher.

## Payload

Each hook receives a single JSON argument as `$1`:

```json
{
  "event": "PostToolUse",
  "session_id": "s_abc",
  "turn": 4,
  "tool": { "name": "run_shell", "is_error": false }
}
```

`tool` is `null` for events that aren't tool-related.

## Exit-code semantics

| Event | Zero exit | Non-zero exit | Timeout |
|---|---|---|---|
| `PreToolUse` | Tool runs | **Tool is skipped** | **Tool is skipped** |
| All others | Continue | Continue (warn logged) | Continue (warn logged) |

Fail-open on PreToolUse **spawn** failures (e.g. typoed command path): a broken hook should never block every tool call. Fail-closed on PreToolUse **timeouts**: a hung hook on a tool-gating event aborts the call to avoid ambiguous state.

## Fault isolation

Hooks **cannot break the agent.** Every failure path resolves to a safe default and a `tracing::warn!` entry:

- Missing / unreadable / malformed `hooks.json` → zero hooks loaded, agent runs normally.
- Unknown event names, invalid matcher regex, missing `command` field → that entry is dropped, other hooks still load.
- Subprocess spawn failure, timeout, non-zero exit, non-UTF-8 output, panic inside the runner → logged, action defaults to `Continue` (or `Skip` for PreToolUse on timeout/non-zero).
- Hook output is capped at 8 KiB per stream. Stdout is discarded; stderr is logged.

Check `~/.borg/logs/tui.log` for hook warnings.

## Configuration

```sh
borg settings set hooks.enabled true   # default: true
borg settings set hooks.enabled false  # disable all user hooks
```

Hooks are loaded once at startup. Edit `hooks.json` then restart `borg` to pick up changes.

## Unsupported in v1

- `"type": "prompt"` and `"type": "agent"` handlers (codex has these; we only support `"command"`).
- Per-project `hooks.json` layering (only the global file at `~/.borg/hooks.json` is read).
- Hot reload.
- Rich payloads (`tool_input`, `tool_result` text, `mutating` flag, sandbox metadata).

## Examples

**Observability** — append failed tool calls to a log:
```json
{ "hooks": { "PostToolUse": [
  { "hooks": [{ "type": "command", "command": "jq -r 'select(.tool.is_error) | \"\\(.session_id) \\(.tool.name)\"' <<< \"$1\" >> ~/.borg/logs/errors.log" }] }
]}}
```

**Gate** — block `run_shell` in specific sessions:
```json
{ "hooks": { "PreToolUse": [
  { "matcher": "run_shell",
    "hooks": [{ "type": "command", "command": "[ \"$(jq -r .session_id <<< \"$1\")\" != \"locked-session\" ]" }] }
]}}
```

**Notify** — ping after each turn:
```json
{ "hooks": { "Stop": [
  { "hooks": [{ "type": "command", "command": "osascript -e 'display notification \"borg turn complete\"'" }] }
]}}
```
