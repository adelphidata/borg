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

Confirm with `cua-driver --version`; the agent searches `PATH` and the usual
install directories by default (see "Driver lookup"). If the binary lives
elsewhere, point `computer_use.driver_path` at it:

```sh
borg settings set computer_use.driver_path /opt/cua/bin/cua-driver
```

## How it reaches the model

`computer_use` lives in the `desktop` tool group (`tool/catalog.rs`). With
`tools.conditional_loading` on (the default) a tool group is advertised only
when the user's message carries one of its keyword hints or the group was
used in the last few turns. The desktop hints are the tool's own name,
"screenshot", "screen", "desktop", "window", "click", "mouse", "keyboard",
"type into", "gui", a few common app names, and "cua". A request that names
none of them (for example "fix the bug in main.rs") does not pay for the
schema. To see what a turn advertised, run the daemon with
`RUST_LOG=cortex::agent::prompt_assembly=debug` and read the
`conditional tool loading` line.

The tool description tells the model that this tool, not shell
`screencapture`/`osascript` and not `browser`, is how it looks at or drives
native apps; the `browser` description says it sees web pages only.

## Driver lookup

`computer_use.driver_path` wins when set. Otherwise the daemon searches
`PATH`, then `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` and
`~/.cargo/bin`. The fallback matters for a daemon the desktop app starts
from the Dock: that process inherits the login session's minimal `PATH`,
which has neither Homebrew nor `~/.local/bin` on it.

## Actions

Every action takes `action`; the other arguments depend on it. The driver is
window-oriented and works in the background: it never raises a window.

### Read-only (plan-mode safe)

| Action         | Args                                                     | Returns |
| -------------- | -------------------------------------------------------- | ------- |
| `capture`      | optional `app` (name substring, or `screen`/`desktop`), `window_id`, `pid`, `mode` (`som` default, `vision`, `ax`) | text + image (`ax`: text only) |
| `list_apps`    | (none)                                                   | running apps with pids |
| `list_windows` | optional `pid`                                           | on-screen windows with `window_id` and `pid` |
| `focus_app`    | required `app`                                           | the resolved `pid` + `window_id`; no raise |
| `wait`         | required `duration_ms`                                   | `waited <N>ms` |

`capture` with no ids resolves the frontmost window matching `app`, or the
frontmost window overall, and reports the resolved `pid` + `window_id` so the
next action can target them. `app="screen"` resolves to the shell window
(Finder or the Dock, matched by whole app name); the driver captures one
window at a time, so when no shell window is listed the error says to
capture the windows you need by `app` instead. In `som` mode the reply
carries the accessibility tree with `[N]` indexes for `element_index`.
`wait` is capped at 5 minutes (300 000 ms).

### Mutating (blocked in plan mode)

All of these require `pid`. An `element_index` additionally requires
`window_id`, the key of the driver's element cache.

| Action          | Args                                                            |
| --------------- | --------------------------------------------------------------- |
| `click`, `double_click`, `right_click`, `middle_click` | `x`,`y` or `element_index`+`window_id`; optional `modifiers` (pixel path only) |
| `drag`          | `x`, `y`, `to_x`, `to_y`; optional `modifiers`                  |
| `scroll`        | `delta_x`, `delta_y` (sign = direction, magnitude = 1–50 keystrokes); optional `element_index`+`window_id` |
| `type`          | `text`; optional `element_index`+`window_id` to focus first     |
| `key`           | `key` (single key) or `keys` (hotkey array)                     |
| `set_value`     | `element_index`, `window_id`, `value`                            |

Pass `capture_after: true` on a mutating action to get a fresh capture in
the same reply.

## What the operator sees

Every `ToolResult` event carries previews of the images the tool returned
(`ToolResultImage`, JPEG, at most 96 KiB each, at most four per call). The
web and desktop transcripts show them inside the expanded tool row, and the
row's subtitle names the action and its target ("click [4]",
"capture Calculator"). The model receives the full-resolution image through
the tool-result message; the preview exists so the transcript stays light
and the per-session event ring (1 024 events) does not fill with screenshots.
Previews ride the live event stream only: the persisted tool message holds
the text, so a conversation reopened from history shows the row without its
picture, the same way the model itself no longer holds the pixels after a
daemon restart.

A text-only main model gets a text summary and a note that pixels were
dropped (`downgrade_for_non_vision_model`). `Provider::supports_vision`
knows the OpenRouter ids by vendor prefix, so a `deepseek/*` model is treated
as text-only up front instead of after a rejected request.

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
