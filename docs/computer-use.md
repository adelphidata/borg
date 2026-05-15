# computer_use — macOS GUI driver

The `computer_use` tool drives the macOS GUI from inside the agent loop. It
spawns the external [`cua-driver`](https://github.com/cua-ai/cua-driver)
binary as an MCP-over-stdio child process per tool call and speaks framed
JSON-RPC 2.0.

This commit adds the **read-only** actions (`capture`, `list_apps`, `wait`).
Mutating actions (`click`, `type`, `key`, `focus_app`, …) land in a follow-up
along with a hard-deny keystroke/target list.

## Install

```sh
brew install cua-driver
```

Confirm with `cua-driver --version`; the agent searches `PATH` by default.
If the binary lives elsewhere, point `computer_use.driver_path` at it:

```sh
borg settings set computer_use.driver_path /opt/cua/bin/cua-driver
```

## Actions

| Action      | Args                            | Returns                                          |
| ----------- | ------------------------------- | ------------------------------------------------ |
| `capture`   | optional `pid`, `window_id`     | `Multimodal { text, parts: [Text, ImageBase64] }` |
| `list_apps` | (none)                          | `Text` — one `Name (pid=N)` per line              |
| `wait`      | required `duration_ms`          | `Text` — `waited <N>ms`                          |

`wait` is capped at 5 minutes (300 000 ms) so a runaway loop can't pin the
worker indefinitely.

## Plan-mode semantics

The whole tool name passes `allowed_in_plan_mode_name`, but the per-action
gate enforced by `Tool::allowed_in_plan_mode` blocks any non-read-only
action when the agent is in Plan mode. With only `capture` / `list_apps` /
`wait` shipped today, every action is permitted. Once mutating actions
land, only the read-only three remain plan-mode-safe.

## Why no approval prompts

Borg's "agent just executes" invariant (`CLAUDE.md` → UX — No Approval
Prompts) applies here. Unlike Hermes' per-call callback, computer_use has
no UI dialog. Safety comes from:

1. The action allowlist (mutating actions ship behind a static denylist —
   see the follow-up commit).
2. Plan-mode gating (read-only actions only when the operator opts in via
   `/plan`).
3. The macOS Accessibility permission grant the host already requires for
   `cua-driver` to function — revoke that and every action fails.

## Why no sandbox-exec wrapper

`computer_use` intentionally bypasses Borg's macOS Seatbelt profile. The
whole point of the tool is to drive *other* applications via Accessibility
APIs — a sandbox that blocks AX would defeat the tool. The action allowlist
is the security boundary instead of the sandbox.

## Debugging

- `Error: \`cua-driver\` not found on PATH` → install via Homebrew or set
  `computer_use.driver_path`.
- `cua-driver error: …` → the driver child returned an MCP-level error
  envelope. Inspect with `cua-driver mcp` directly to reproduce.
- Empty multimodal envelope on capture → the driver returned no `image`
  content. Re-snapshot; if persistent, check macOS screen-recording
  permissions for the borg daemon.
