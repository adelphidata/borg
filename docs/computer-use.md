# computer_use — macOS GUI driver

The `computer_use` tool drives the macOS GUI from inside the agent loop. It
spawns the external [`cua-driver`](https://github.com/cua-ai/cua-driver)
binary as an MCP-over-stdio child process per tool call and speaks framed
JSON-RPC 2.0.

Ships with both read-only actions (`capture`, `list_apps`, `wait`) and
mutating actions (`click` family, `drag`, `scroll`, `type`, `key`,
`set_value`, `focus_app`). A hard denylist refuses logout/lock/force-quit
hotkeys, fork-bomb-shaped `type` payloads, and `focus_app` targets that
match the macOS login window or Security settings — checked **before** the
driver is spawned.

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

### Read-only (plan-mode safe)

| Action      | Args                            | Returns                                          |
| ----------- | ------------------------------- | ------------------------------------------------ |
| `capture`   | optional `pid`, `window_id`     | `Multimodal { text, parts: [Text, ImageBase64] }` |
| `list_apps` | (none)                          | `Text` — one `Name (pid=N)` per line              |
| `wait`      | required `duration_ms`          | `Text` — `waited <N>ms`                          |

`wait` is capped at 5 minutes (300 000 ms) so a runaway loop can't pin the
worker indefinitely.

### Mutating (blocked in plan mode)

| Action          | Args                                  | Returns          |
| --------------- | ------------------------------------- | ---------------- |
| `click`         | required `x`, `y`                     | `Text` confirmation |
| `double_click`  | required `x`, `y`                     | `Text` |
| `right_click`   | required `x`, `y`                     | `Text` |
| `middle_click`  | required `x`, `y`                     | `Text` |
| `drag`          | required `x`, `y`, `to_x`, `to_y`     | `Text` |
| `scroll`        | required `x`, `y`, `delta_x`, `delta_y` | `Text` |
| `type`          | required `text`                       | `Text` |
| `key`           | one of `key` (string) or `keys` (array) | `Text` |
| `set_value`     | required `element_index`, `value`     | `Text` |
| `focus_app`     | required `target`; optional `raise_window` (default `true`) | `Text` — `raised` or `backgrounded` |

`focus_app(raise_window: false)` is the opt-in "drive in the background"
path — input routes to the target app without stealing focus from the
user's frontmost window. Default `true` keeps the safer-by-visibility
behaviour where the user can see what the agent is doing.

## Denylist

The denylist runs in `check_denylist` (`tool/computer_use.rs`) **before**
the driver is spawned, mirroring `policy::HARDCODED_DENY` for shell
commands. Refused inputs surface a clear error to the LLM so it can
re-plan without burning budget probing what's blocked.

**Hotkeys** — normalised to lowercased, alphabetically sorted modifier
list so `cmd+shift+q` and `shift+cmd+q` both match:

- Cmd+Shift+Q — would log the user out of their macOS session.
- Cmd+Ctrl+Q — would lock the screen.
- Cmd+Option+Esc — opens the force-quit menu.

**`type` payloads** — fork-bomb-shaped function-definitions that pipe
themselves (`:(){:|:&};:` and near-cousins) are rejected via a static
regex. The agent should reach for `run_shell` rather than typing shell
into an open terminal anyway.

**`focus_app` targets** — `loginwindow`, `Screen Lock`, `ScreenSaver`,
`System Settings`, `System Preferences`, and `Security & Privacy` are
treated as macOS security surfaces that must never be driven.

## Plan-mode semantics

The tool name passes `allowed_in_plan_mode_name` (which is name-level
only), but `Tool::allowed_in_plan_mode` discriminates per-action against
`ComputerUseAction::is_readonly`. In Plan mode only `capture`, `list_apps`,
and `wait` reach the driver; every mutating action surfaces a plan-mode
block at dispatch time.

## Why no approval prompts

Borg's "agent just executes" invariant (`AGENTS.md` → UX — No Approval
Prompts) applies here. `computer_use` has no UI dialog — including for
mutating actions. Safety comes from:

1. The static denylist (logout/lock/force-quit/fork-bomb/security-surface
   refusal before driver dispatch).
2. Plan-mode gating (read-only actions only when the operator opts in via
   `/plan`).
3. The macOS Accessibility permission grant the host already requires for
   `cua-driver` to function — revoke that and every action fails.
4. The user can revoke screen recording / Accessibility / Input Monitoring
   in System Settings → Privacy & Security at any time and the tool stops
   working immediately.

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
