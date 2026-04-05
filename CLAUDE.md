# Borg

AI personal assistant agent built in Rust. The agent itself is the plugin system — it writes its own tools at runtime rather than relying on a static extension framework.

## UX Philosophy — Tool Execution

**DO NOT add per-tool-call approval prompts or confirmation dialogs.** The power of a personal AI assistant is that it can act on your behalf. Security should never degrade UX to the point where every action requires manual approval.

- Shell commands auto-execute without prompting. Only hardcoded catastrophic commands (rm -rf /, mkfs, dd, curl|sh) are denied.
- No HITL (human-in-the-loop) confirmation for tool calls — the agent just executes.
- Sandboxing and rate limiting are the security boundaries, not approval dialogs.
- If you're tempted to add a "confirm before executing" flow, don't. This has been explicitly removed as a design decision.

## Architecture

Cargo workspace with 8 crates:

```
crates/
  cli/              Binary: REPL, clap args, heartbeat display, onboarding TUI
  core/             Library: agent loop, multi-provider LLM client, memory, identity, config
  heartbeat/        Library: proactive scheduler with quiet hours + dedup
  sandbox/          Library: macOS Seatbelt + Linux Bubblewrap policies, script runner
  apply-patch/      Library: patch DSL parser + filesystem applicator
  gateway/          Library: webhook gateway for messaging channel integrations
  plugins/          Library: marketplace catalog, plugin installer, TUI integration
```

**Data directory:** `~/.borg/` — config, personality, memory, scripts, logs.

## Build & Test

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy -- -D warnings
```

Binary name is `borg`. Requires one of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, a running Ollama instance, or Claude Code CLI with valid subscription at runtime (see `.env.example`).

All integrations are compiled unconditionally into a single binary. iMessage is macOS-only via `#[cfg(target_os = "macos")]`.

Smaller library crates (apply-patch, sandbox, heartbeat, plugins) enforce `#![warn(missing_docs)]`. Core and gateway crates have doc comments on key public APIs — add `///` doc comments to any new public item.

## Code Coverage

Target: **80%+ test coverage**. Coverage is reported via Codecov on every PR. To generate locally:

```sh
just coverage          # HTML report
just coverage-summary  # text summary
```

Requires `cargo-llvm-cov` (`cargo install cargo-llvm-cov`).

## Installation

```sh
curl -fsSL https://raw.githubusercontent.com/borganization/borg/main/scripts/install.sh | bash
```

The installer detects OS/arch, downloads a pre-built binary from GitHub Releases, verifies checksums, installs to `~/.local/bin/`, and runs `borg init` for first-time setup.

Release binaries are built via `.github/workflows/release.yml` on tag push (`v*`) for macOS (x86_64, arm64) and Linux (x86_64, arm64).

## CLI Commands

- `borg` or `borg chat` — interactive REPL
- `borg ask "message"` — one-shot query
- `borg --version` — show version
- `borg init` — interactive onboarding wizard (Welcome → Security → Provider → API Key → Channels → Summary)
- `borg add <name>` — set up an integration's credentials (e.g. `borg add telegram`)
- `borg remove <name>` — remove an integration's credentials
- `borg plugins` — list all integrations with configured/unconfigured status
- `borg gateway` — start webhook gateway server for messaging channels
- `borg poke` — trigger an immediate heartbeat check-in (sends poke signal to daemon)
- `borg status` — show agent vitals (stability, focus, sync, growth, happiness)
- `borg doctor` — run diagnostics (config, provider, sandbox, tools, skills, memory, gateway, budget, host security)
- `borg tasks list` — list all scheduled tasks
- `borg tasks create` — create a scheduled task (supports `--max-retries`, `--timeout`, `--delivery-channel`, `--delivery-target`)
- `borg tasks run <id>` — trigger a task to run immediately
- `borg tasks runs <id>` — show execution history for a task
- `borg tasks status <id>` — show detailed task status including retry state and delivery config
- `borg tasks pause/resume/delete <id>` — manage task lifecycle
- `borg cron list` — list all cron jobs
- `borg cron add "*/5 * * * * echo hello"` — add a cron job (combined crontab format)
- `borg cron add -s "*/5 * * * *" -c "echo hello"` — add a cron job (separate flags)
- `borg cron remove/pause/resume/run <id>` — manage cron job lifecycle
- `borg cron runs <id>` — show execution history for a cron job
- `borg update` — update borg to latest stable release (supports `--dev` for pre-release, `--check` for check-only)
- `/update` (TUI command) — update borg to latest version (`/update --dev` for pre-release)
- `/poke` (TUI command) — trigger an immediate heartbeat check-in
- `/plugins` (TUI command) — open marketplace popup to install/uninstall messaging, email, and productivity integrations

## Onboarding

Opinionated QuickStart flow — one streamlined path with sensible defaults:

1. **Welcome** — User name + agent name
2. **Security** — Security warning acknowledgment (required)
3. **Provider** — Select LLM provider (OpenRouter, OpenAI, Anthropic, Gemini, Ollama)
4. **API Key** — Enter API key (auto-detects existing keys)
5. **Channels** — Configure messaging channels (Telegram, Slack, Discord, etc.)
6. **Summary** — Review all settings including defaults, confirm and launch

Defaults applied automatically: Professional personality, recommended model per provider, 1M token/month budget, gateway at 127.0.0.1:7842, strict sandbox. Customize via `borg settings`.

After onboarding, SETUP.md is created with first-conversation instructions so the agent can introduce itself and get to know the user. SETUP.md is injected into the system prompt once, then deleted.

## Mouse Interaction

**Native click+drag text selection MUST work in the transcript with no modifier keys.** This is critical UX and has regressed multiple times — treat it as a hard invariant.

**Strategy — xterm Alternate Scroll Mode (`?1007h`), nothing else.**
The TUI enables ONLY `?1007h` via the custom `EnableAlternateScroll` command in `crates/cli/src/tui/mod.rs`. In alternate screen, the terminal itself translates mouse-wheel events into `CUR_UP` / `CUR_DOWN` key sequences that arrive as normal `KeyCode::Up` / `KeyCode::Down`. Because no mouse tracking mode is enabled, click+drag stays handled entirely by the terminal — exactly like `less` or `vim`. Reference implementation: `reference/codex/codex-rs/tui/src/tui.rs`.

**Behavior:**
- Click+drag anywhere in the transcript selects text natively (Cmd/Ctrl+C copies). No Option/Fn/Shift modifier needed.
- Mouse wheel scrolls the transcript one line per tick (via terminal-level wheel→arrow translation).
- `PageUp` / `PageDown` scroll the transcript in 20-line jumps.
- `Up` / `Down` arrows are routed by `App::handle_key` using dual semantics:
  - If `scroll_offset > 0` (reading scrollback) → scroll transcript one line.
  - Else if composer is idle (empty AND not browsing history) AND transcript is scrollable → scroll transcript (wheel semantics win from the bottom).
  - Else → composer history navigation (shell-style recall).
- `Ctrl+P` / `Ctrl+N` always navigate composer history regardless of scroll state.
- Scrollbar click/drag is intentionally NOT supported — it would require `?1000h`, which breaks selection.

**FORBIDDEN — any of these will regress text selection:**
- Escape sequences: `?1000h` (button tracking), `?1002h` (drag tracking), `?1003h` (any-event tracking), `?1006h` (SGR coordinates — only meaningful with the above).
- Crossterm API: `EnableMouseCapture`, `DisableMouseCapture` (they enable `?1000h`+`?1002h`+`?1003h`+`?1006h`).
- `Event::Mouse` match arms in the event loop — there is no mouse event source.
- `App::handle_mouse` or `MouseEventKind` references in `app.rs`.

**Guard tests (do not remove or weaken):**
- `crates/cli/src/tui/mod.rs` — escape-sequence correctness (`enable_alternate_scroll_emits_exactly_1007h`, etc.), symmetry check, and source-level guards (`mod_rs_code_contains_no_forbidden_mouse_modes`, `app_rs_code_contains_no_forbidden_mouse_modes`, `mod_rs_code_does_not_call_crossterm_enable_mouse_capture`, `app_rs_has_no_event_mouse_match_arm`, `app_rs_has_no_mouse_event_kind_references`) that read both files via `include_str!` and fail the build if any forbidden pattern is reintroduced.
- `crates/cli/src/tui/app.rs` — arrow-routing tests cover all three rules (scroll-while-scrolled-up, wheel-from-bottom, active-composer-history), plus `Ctrl+P` escape hatch, wheel simulation, and regression guards for PageUp/PageDown and printable input.

## Plugins

Plugin marketplace for one-click installation of channel and tool integrations. Categories: Messaging (Telegram, Slack, Discord, Teams, Google Chat, WhatsApp, iMessage, SMS), Email (Gmail, Outlook), Productivity (Google Calendar, Notion, Linear). Native integrations (Telegram, Slack, Discord, Teams, Google Chat) are marked `is_native: true` in the catalog and require only credentials. Non-native plugins use embedded template files installed to `~/.borg/channels/`.

## Agent Loop

`core/agent.rs` — streams LLM response, parses tool calls, executes them, appends results, loops until text-only response.

- **Internal tag stripping**: `<internal>...</internal>` blocks are stripped from output in real-time during streaming (prevents chain-of-thought leakage)
- **Message persistence**: Each message is written to SQLite (`messages` table) immediately when added to history, enabling crash recovery
- **Message timestamps**: All messages carry RFC3339 timestamps for temporal reasoning and compaction summaries

System prompt assembled each turn: `IDENTITY.md` + current time + git context + collaboration mode + memory context + project docs + coding instructions + skills context (all token-budgeted).

## Collaboration Modes

Three modes that change how the agent interacts, set via config, `/mode` TUI command, or `--mode` CLI flag:

- **Default** — Standard collaborative mode. Asks questions when needed.
- **Execute** — Autonomous mode. Makes assumptions, executes independently, reports progress via checklists.
- **Plan** — Read-only exploration mode. Gathers info, asks questions, produces a `<proposed_plan>`, blocks all mutating tools.

Templates in `crates/core/templates/collaboration_mode/`. Mode is injected into system prompt as `<collaboration_mode>`.

Plan mode uses an allowlist of non-mutating tools — new tools default to blocked.

**Config:** `[conversation] collaboration_mode = "default"` (or `execute`, `plan`)
**TUI:** `/mode execute`, `/mode plan`, `/mode default` (or Shift+Tab to cycle)
**CLI:** `borg ask --mode execute "do the thing"`

**`/plan` shortcut:** `/plan` is a shortcut for `/mode plan` — it toggles Plan mode on and stashes the prior mode. `/plan <message>` enters Plan mode and submits the message in one step. When a Plan-mode turn completes, the TUI pops a review overlay (`PlanOverlay` in `crates/cli/src/tui/plan_overlay.rs`) with Clear & Proceed / Proceed with Context / Type Feedback options. Choosing Proceed automatically restores the prior collaboration mode so the follow-up execution turn is not blocked by mutation constraints. `App::previous_collab_mode` in `crates/cli/src/tui/app.rs` is the single source of truth for the "transient Plan → execute" flow — there is no separate `plan_mode` TUI flag.

## Git Utilities

`crates/core/src/git.rs` — coding agent safety net.

- **Ghost commits**: On session start, creates a snapshot of the entire repo using a temp git index (never touches HEAD or user's index). Enables atomic undo via `restore_ghost_commit`.
- **Git context**: Enriches system prompt with branch, commit hash, recent commits, uncommitted changes status.

## Project Doc Discovery

`crates/core/src/project_doc.rs` — walks from CWD up to git root, collects `AGENTS.md` and `CLAUDE.md` files (first match per directory), concatenates root-first, injects into system prompt as `<project_instructions>`. Budget: 32 KiB.

## Built-in Tools

**IMPORTANT: Be conservative adding new tools.** Every tool's JSON schema is sent to the LLM every turn, directly consuming context tokens (~5KB+ per tool). Prefer adding actions/parameters to an existing tool over creating a new one. If a capability can be achieved via `run_shell` or an existing tool with an extra action, don't create a new tool.

| Tool | Purpose |
|------|---------|
| `write_memory` | Write/append to memory files (IDENTITY.md, MEMORY.md, or topic files). Supports `scope` param (`global`/`local`) |
| `read_memory` | Read a memory file |
| `list` | List resources (tools, skills, channels, agents) |
| `apply_patch` | Create/update/delete files via patch DSL. Target: cwd (default), skills, or channels |
| `run_shell` | Execute a shell command |
| `read_file` | Read file contents with line numbers, image rendering, PDF extraction |
| `list_dir` | List directory contents with types and sizes. Supports depth recursion (max 3) and hidden files. Security: checks blocked paths on every entry |
| `browser` | Headless Chrome automation (navigate, click, type, screenshot, get_text, evaluate_js, close). Requires `browser.enabled = true` |
| `schedule` | Manage scheduled jobs — both AI prompt tasks (type=prompt) and shell cron jobs (type=command). Actions: create, list, get, update, pause, resume, cancel, delete, runs, run_now |

## Patch DSL

Used by `apply_patch` to create/modify/delete files. Follows the codex apply-patch format where **every content line must have a prefix** (`+` for added content, ` ` for context, `-` for removed lines). This prevents ambiguity when file content contains `***` markers.

```
*** Begin Patch
*** Add File: path/to/file.py
+import sys
+print("hello")
*** Update File: path/to/file.py
@@
 context
-old line
+new line
*** Delete File: path/to/old.py
*** End Patch
```

## Config

`~/.borg/config.toml`:

```toml
[llm]
provider = "openrouter"             # openrouter | openai | anthropic | gemini | deepseek | groq | ollama | claude-cli (auto-detected if omitted)
api_key_env = "OPENROUTER_API_KEY"
model = "anthropic/claude-sonnet-4"
temperature = 0.7
max_tokens = 4096
# base_url = "https://custom-endpoint/v1/chat/completions"  # optional: override provider's default URL

[heartbeat]
interval = "30m"
cron = "0 */30 * * * *"          # optional, overrides interval
quiet_hours_start = "00:00"
quiet_hours_end = "06:00"
# channels = ["telegram"]        # deliver heartbeat to channels (empty = TUI only)

[tools]
default_timeout_ms = 30000

[sandbox]
enabled = true
mode = "strict"

[memory]
max_context_tokens = 8000
flush_before_compaction = false  # extract durable info before compaction
flush_min_messages = 4           # minimum dropped messages to trigger flush
# extra_paths = ["~/notes", "~/docs"]  # additional dirs to index and search

[memory.embeddings]
enabled = true                   # enable semantic memory search
# provider = "openai"           # optional: override (auto-detects from API keys)
# model = "text-embedding-3-small" # optional: override embedding model
# dimension = 1536              # optional: override vector dimension
# api_key_env = "OPENAI_API_KEY" # optional: override API key env var
recency_weight = 0.2            # 0.0=pure similarity, 1.0=pure recency
mmr_enabled = true               # MMR diversity re-ranking of search results
mmr_lambda = 0.7                 # 1.0=pure relevance, 0.0=pure diversity

[skills]
enabled = true
max_context_tokens = 4000

[security]
secret_detection = true
blocked_paths = [".ssh", ".aws", ".gnupg", ".config/gh", ".env", "credentials", "private_key"]
host_audit = true                # enable host security checks in doctor command
action_limits.tool_calls_warn = 50
action_limits.tool_calls_block = 100
action_limits.shell_commands_warn = 20
action_limits.shell_commands_block = 50
action_limits.file_writes_warn = 15
action_limits.file_writes_block = 30
action_limits.memory_writes_warn = 10
action_limits.memory_writes_block = 20
action_limits.web_requests_warn = 20
action_limits.web_requests_block = 50

[budget]
monthly_token_limit = 1000000    # 0 = unlimited
warning_threshold = 0.8          # warn at 80% usage

[browser]
enabled = true                   # enable/disable browser automation
headless = true                  # run headless (no visible window)
# executable = "/path/to/chrome" # optional: override auto-detected Chrome path
cdp_port = 9222                  # Chrome DevTools Protocol port
no_sandbox = false               # disable Chrome sandboxing (containers)
timeout_ms = 30000               # default command timeout
startup_timeout_ms = 15000       # browser launch timeout

[credentials]
JIRA_API_TOKEN = "JIRA_API_TOKEN"                    # legacy: bare string = env var name
SLACK_BOT_TOKEN = { source = "exec", command = "security", args = ["find-generic-password", "-s", "slack", "-w"] }
GITHUB_TOKEN = { source = "file", path = "~/.config/gh/token" }
MY_SECRET = { source = "env", var = "MY_SECRET" }    # explicit env SecretRef
```

## Memory System

- `~/.borg/MEMORY.md` — loaded every turn
- `~/.borg/memory/*.md` — loaded by relevance (semantic search) or recency until token budget exhausted
- **Per-project local memory**: `$CWD/.borg/memory/*.md` — loaded in addition to global memory when present
- `write_memory` tool accepts `scope: "local"` to write to project-local memory
- Token estimation via `tiktoken-rs` (cl100k_base BPE tokenizer)
- **Semantic search**: Embeddings generated on `write_memory` and stored in SQLite. Memory files ranked by cosine similarity to the user's query, blended with recency. Auto-detects embedding provider from API keys (OpenAI → OpenRouter → Gemini). Silently falls back to recency when no embedding provider available (e.g., Anthropic-only users). Configure via `[memory.embeddings]` in config.
- **Hybrid search**: `memory_search` tool combines vector similarity (70%) and BM25 full-text search (30%) with adaptive weighting — when one signal is unavailable, the other scales to full weight
- **Markdown-aware chunking**: Chunks respect code fences (never split mid-fence) and use headings as natural chunk boundaries
- **MMR diversity re-ranking**: Search results are re-ranked using Maximal Marginal Relevance (Jaccard similarity) to reduce redundant results. Configure via `mmr_enabled` and `mmr_lambda`
- **Embedding cache**: API results cached in SQLite by (provider, model, content_hash) to avoid redundant embedding API calls
- **Extra search paths**: `memory.extra_paths` config indexes additional directories alongside regular memory (validated against `security.blocked_paths`)
- **Memory file watcher**: Background watcher auto-re-indexes .md files when modified on disk (1.5s debounce)
- **Session transcript indexing**: Past conversations are indexed on startup and searchable via `memory_search` under the "sessions" scope
- **Pre-compaction memory flush**: When enabled (`flush_before_compaction`), extracts durable information from messages about to be dropped and saves to `daily/{date}.md`

## Identity (IDENTITY.md)

`~/.borg/IDENTITY.md` is injected into the system prompt. The agent can update it via `write_memory` targeting `IDENTITY.md`. Changes persist across sessions.

During `borg init`, the onboarding wizard generates a personalized IDENTITY.md based on the user's chosen agent name and personality style (Professional, Casual, Snarky, Nurturing, or Minimal).

## Skills

Skills are instruction bundles (SKILL.md files with YAML frontmatter) that teach the agent how to use external CLI tools via `run_shell`. Distinct from "tools" which are sandboxed executable scripts. Built-in skills are embedded via `include_str!` and always compiled in. During `borg init`, bundled skills are extracted to `~/.borg/skills/`.

- **Built-in skills**: Embedded via `include_str!` in `crates/core/skills/*/SKILL.md` (slack, discord, github, weather, skill-creator, git, http, search, docker, database, notes, calendar, 1password, browser)
- **User skills**: `~/.borg/skills/<name>/SKILL.md` — created via `apply_skill_patch`
- User skills with the same name override built-in skills
- Requirements (bins/env vars) are checked at load time against both process env and `[credentials]` store; unavailable skills are still listed but flagged
- **Credential injection**: Resolved credentials from `[credentials]` are injected as env vars into `run_shell` commands, so skills can reference them without the user exporting them in the shell
- **Progressive loading**: Metadata (name + description) always loaded for all skills (~50 tokens each); full SKILL.md body loaded only for available skills within token budget
- **References**: User skills can have `references/*.md` files stored on the Skill struct (not auto-loaded into context)
- **Scripts**: User skills can have `scripts/` directories; paths stored on the Skill struct for use with `run_shell`

**SKILL.md format:**
```markdown
---
name: my-skill
description: "What it does and when to use it."
requires:
  bins: ["curl"]
  env: ["API_TOKEN"]
---

# Skill Title

Instructions and command examples here.
```

## Messaging Channels (Gateway)

Channels are user-created integrations that receive webhooks from external services, route messages to the agent, and send responses back. They follow the same pattern as tools.

**Native integrations:** Telegram and Slack are handled natively in Rust (`crates/gateway/src/telegram/`, `crates/gateway/src/slack/`) using `reqwest` — no Python scripts needed. Set `TELEGRAM_BOT_TOKEN` or `SLACK_BOT_TOKEN`/`SLACK_SIGNING_SECRET` env vars (or configure via `[credentials]` store). Telegram optionally uses `gateway.public_url` for automatic webhook registration.

**Location:** `~/.borg/channels/<name>/`

**channel.toml format:**
```toml
name = "my-slack"
description = "Slack workspace integration"
runtime = "python"              # python | node | deno | bash

[scripts]
inbound = "parse_inbound.py"   # Receives {headers, body} JSON -> normalized message
outbound = "send_outbound.py"  # Receives {text, sender_id, channel_id, token} JSON -> sends
verify = "verify.py"           # Optional: webhook signature verification

[sandbox]
network = true
fs_read = ["/etc/ssl"]
fs_write = []

[auth]
secret_env = "SLACK_SIGNING_SECRET"  # For webhook verification
token_env = "SLACK_BOT_TOKEN"        # Passed to outbound script

[settings]
webhook_path = "/webhook/my-slack"   # Default: /webhook/<name>
timeout_ms = 15000
max_concurrent = 5
```

**Message flow:** External Service -> POST /webhook/<name> -> verify -> parse inbound -> agent -> outbound script

**Config:**
```toml
[gateway]
host = "127.0.0.1"
port = 7842
max_concurrent = 10
request_timeout_ms = 120000
```

**CLI:** `borg gateway` starts the gateway server standalone. The gateway also runs automatically as part of the daemon.

**Built-in tools:** `create_channel` (patch DSL to `~/.borg/channels/`), `list_channels`

**Database:** V3 migration adds `channel_sessions` and `channel_messages` tables.

### Thread-Scoped Conversation History

The DB-level session key is composed as `{sender_id}:{thread_id}` in
`crates/gateway/src/handler.rs` (~L372), so each thread from the same sender
gets its own conversation history. Parsers are responsible for populating
`InboundMessage.thread_id` from the platform's native thread identifier:

- **Slack** — `event.thread_ts` (top-level messages leave it `None`)
- **Teams** — `activity.reply_to_id` (top-level messages leave it `None`)
- **Discord** — `interaction.channel_id` (Discord threads are first-class channels with distinct IDs)
- **Google Chat** — `message.thread.name`
- **Telegram** — `message.message_thread_id` (forum topics)

### Gateway Bindings (Per-Channel Overrides)

`crates/gateway/src/routing.rs` resolves a per-message binding with a 4-tier
cascade (`channel+sender+peer_kind` → `channel+sender` → `channel` →
`default`). A matched binding can override `provider`, `model`, `api_key_env`,
`temperature`, `max_tokens`, `fallback`, `memory_scope`, `identity`,
`activation`, and `thinking`. This is the idiomatic way to route different
channels to different LLMs:

```toml
[[gateway.bindings]]
channel = "slack"
sender  = "U12345*"    # glob prefix
provider = "anthropic"
model = "claude-sonnet-4"
memory_scope = "work"

[[gateway.bindings]]
channel = "telegram"
provider = "openrouter"
model = "google/gemini-2.5-flash"
```

Heartbeat delivery also honors bindings: when a heartbeat channel matches a
binding, the daemon runs an additional per-channel heartbeat turn with that
binding's overridden config, so proactive check-ins reach each channel via the
model it was routed to. Channels without a matching binding reuse the base
turn (fast path, no extra LLM calls).

## Sender Pairing (Access Control)

Gateway messages from unknown senders are gated behind a pairing approval system. When an unapproved sender messages the bot, they receive a pairing code challenge. The bot owner approves via CLI or TUI.

**Flow:** Unknown sender → bot replies with pairing code + sender ID → owner runs `borg pairing approve <channel> <CODE>` → sender added to approved list.

**DM Policy** (`dm_policy`): `pairing` (default, require approval) | `open` (allow all) | `disabled` (reject all). Per-channel overrides via `channel_policies` map.

**Config:**
```toml
[gateway]
dm_policy = "pairing"           # pairing | open | disabled
pairing_ttl_secs = 3600         # code expiration (default 60 min)

[gateway.channel_policies]
telegram = "pairing"
slack = "open"                  # trust Slack workspace auth
```

**CLI commands:**
- `borg pairing` or `borg pairing list [channel]` — list pending pairing requests
- `borg pairing approve <channel> <CODE>` — approve a sender by pairing code
- `borg pairing revoke <channel> <sender_id>` — revoke an approved sender
- `borg pairing approved [channel]` — list all approved senders

**TUI command:** `/pairing` — shows pending requests and approved senders inline.

**Database:** V13 migration adds `pairing_requests` and `approved_senders` tables.

**Interception point:** `handler::invoke_agent()` in `crates/gateway/src/handler.rs` — single check for all channels (Telegram, Slack, Discord, Teams, Google Chat, Twilio, script-based).

## Lifecycle Hooks

`crates/core/src/hooks.rs` — extensible hook system for intercepting agent loop events.

**Hook points**: `BeforeAgentStart`, `BeforeLlmCall`, `AfterLlmResponse`, `BeforeToolCall`, `AfterToolCall`, `TurnComplete`, `OnError`

**Hook actions**: `Continue` (no-op), `InjectContext(String)` (append to system prompt), `Skip` (skip current action, e.g. tool call)

Hooks implement the `Hook` trait and are registered on the agent via `agent.hook_registry_mut().register(...)`. Multiple hooks can be registered; `InjectContext` results are merged, `Skip` short-circuits.

## Settings System

Three-tier resolution (highest priority wins): **Database** → **config.toml** → **Compiled defaults**.

Settings are persisted in SQLite (`settings` table) and applied as runtime overrides on top of `config.toml`. Users can configure via:
- `/settings` TUI popup (interactive toggle/edit)
- `borg settings set KEY VALUE` / `borg settings get KEY` / `borg settings unset KEY` (CLI)
- Direct `config.toml` edits (for array/nested config like provider fallback chains)

### Adding a New Setting (4 touch points)

1. **`config.rs`** — Add field to the relevant config struct + `apply_setting()` match arm with validation
2. **`settings.rs`** — Add key to `ALL_SETTING_KEYS` array + `config_value_for_key()` match arm
3. **`settings_popup.rs`** — Add `SettingEntry` to `SETTINGS` array (key, label, kind, category)
4. Tests in each file verifying parse/resolve/display

The `SettingsResolver` handles merging automatically — no additional wiring needed. `SettingKind` options: `Bool` (Space toggles), `Float` (arrows ±0.1), `Uint` (Enter to edit), `Text` (Enter to edit), `Select` (Left/Right cycle).

### Key Files

| File | Role |
|------|------|
| `crates/core/src/settings.rs` | Resolver: DB → TOML → defaults, `ALL_SETTING_KEYS`, `config_value_for_key()` |
| `crates/core/src/config.rs` | `Config::apply_setting()` — validates and applies key/value pairs |
| `crates/cli/src/tui/settings_popup.rs` | TUI popup: `SETTINGS` array, interactive editing, DB persistence |
| `crates/core/src/db.rs` | SQLite `settings` table: `get_setting`, `set_setting`, `delete_setting`, `list_settings` |
| `crates/cli/src/main.rs` | CLI `borg settings` subcommands |

## Vitals System

`crates/core/src/vitals.rs` — passive agent health tracking via lifecycle hooks.

Five stats (stability, focus, sync, growth, happiness) update automatically from usage events classified into broad categories (Interaction, Success, Failure, Correction, Creation). State is **event-sourced** — computed by replaying verified events from baseline, not stored mutably. HMAC-SHA256 chain prevents tampering; per-category-per-hour rate limiting prevents gaming.

`VitalsHook` implements the `Hook` trait, listens on `SessionStart`, `BeforeAgentStart`, and `AfterToolCall`, and appends events to SQLite.

**Commands:** `borg status` (CLI), `/status` (TUI). TUI shows compact vitals header on session start.

**DB tables (V22):** `vitals_events` (append-only ledger with HMAC chain).

See `docs/vitals.md` for full documentation.

## Doctor Command

`crates/core/src/doctor.rs` — diagnostic checks for config, provider, sandbox, tools, skills, memory, data directory, budget, and host security.

Available via `borg doctor` CLI subcommand or `/doctor` TUI command.

## Database

SQLite at `~/.borg/borg.db` with versioned migrations:
- **V1**: sessions, scheduled_tasks, task_runs, meta, token_usage tables
- **V2**: messages table (crash recovery), retry_count on scheduled_tasks
- **V3**: channel_sessions + channel_messages tables (gateway)
- **V14**: Add retry (max_retries, retry_count, retry_after, last_error), timeout (timeout_ms), and delivery (delivery_channel, delivery_target) columns to `scheduled_tasks`
- **V16**: `embedding_cache` table for caching embedding API results
- **V17**: `session_index_status` table for tracking indexed sessions
- **V22**: `vitals_events` table (event-sourced agent health tracking with HMAC chain)
- Schema version tracked in `meta` table; migrations run automatically on `Database::open()`

## Signal Handling & Graceful Shutdown

- Global `CancellationToken` wired to SIGINT (Ctrl+C) and SIGTERM (Unix)
- Daemon drains in-progress tasks via semaphore before exiting
- Agent loop respects cancellation between iterations

## Daemon & Concurrent Tasks

- Daemon uses `tokio::sync::Semaphore` (capacity from `tasks.max_concurrent`, default 3)
- Each task spawned as independent tokio task with per-job timeout (default 300s, configurable via `timeout_ms`)
- Failed tasks are recorded with error details in `task_runs` table
- **Retry with backoff**: Transient failures (timeout, rate limit, 5xx, connection errors) are retried up to `max_retries` (default 3) with exponential backoff (30s → 60s → 5m → 15m → 1h). Non-transient errors skip retry.
- **Result delivery**: Tasks with `delivery_channel` and `delivery_target` send results (or failure notifications) to Telegram, Slack, or Discord after execution. Use `delivery_channel = "origin"` when scheduling from a chat conversation to reply back into the same channel/thread — the gateway handler stashes the origin context in a task-local (`borg_core::gateway_context`), and the `schedule` tool resolves it at creation time into the real channel + a JSON `delivery_target` carrying sender/thread so the reply threads correctly.
- **Missed job catch-up**: On daemon startup, tasks overdue by >7 days are skipped and advanced to next run.

## Cron Jobs

Linux-style cron jobs that execute shell commands directly (no LLM involved). Uses the same `scheduled_tasks` table with `task_type = "command"` to distinguish from prompt tasks (`task_type = "prompt"`).

- **CLI**: `borg cron add "*/5 * * * * echo hello"` or `borg cron add -s "*/5 * * * *" -c "echo hello"`
- **Agent tool**: `schedule` with type=command and actions: create, list, get, delete, pause, resume, runs, run_now
- **Schedule format**: 5-field Linux cron (`min hour dom month dow`), auto-converted to 7-field internally
- **Execution**: Direct `sh -c <command>` via `tokio::process::Command` — no LLM client needed
- **Output**: stdout/stderr captured and stored in `task_runs.result`; non-zero exit stored in `task_runs.error`
- **No retry on non-zero exit**: Unlike prompt tasks, script failures are not retried (they're user errors, not transient)
- **Daemon**: Same daemon loop picks up both prompt and command tasks; branches on `task_type`

## Heartbeat

Proactive check-in system. The `HeartbeatScheduler` (pure timer) emits `Fire` events on schedule; the consumer (daemon or TUI) runs a full agent turn with tools, then delivers to configured channels.

- **Scheduling**: Interval-based (default `30m`) or cron-based; minimum 60s enforced
- **Quiet hours**: Default `00:00`–`06:00`, uses `[user] timezone` from config (IANA string, e.g. `America/New_York`)
- **HEARTBEAT.md**: Optional checklist at `~/.borg/HEARTBEAT.md`; injected into the heartbeat agent turn so the agent can check email, calendar, etc.
- **Channel delivery**: `heartbeat.channels` list (e.g. `["telegram"]`). Recipient selection: `heartbeat.recipients.<channel>` is a list of sender IDs (use `["*"]` to broadcast to every approved sender for that channel); absence falls back to the first approved sender.
- **Per-channel model override**: Heartbeat honors gateway bindings — if a binding matches `(channel, sender)`, the daemon re-runs the turn with the binding's overridden provider/model so check-ins reach each channel via the model routed to it.
- **Suppression**: Empty responses, duplicate hash responses, and zero-info ack-only responses ("ok", "nothing new", "all good", etc.) are suppressed
- **Poke**: `borg poke` (CLI) or `/poke` (TUI) triggers an immediate heartbeat (bypasses quiet hours). CLI sends HTTP POST to `/internal/poke` on the gateway; TUI sends directly via channel when owning the scheduler
- **Daemon**: Heartbeat is spawned in `run_daemon()` alongside the gateway; runs without a TUI
- **TUI**: Heartbeat renders in cyan as `[heartbeat]` prefix in the transcript

## Sandboxing

Scripts and channel integrations run sandboxed:
- **macOS**: `sandbox-exec` with generated Seatbelt profile (deny-all default, explicit allows)
- **Linux**: `bwrap` with namespace isolation (read-only mounts, network unshare)
- **Filesystem blocklist**: Paths in `[security] blocked_paths` (defaults: `.ssh`, `.aws`, `.gnupg`, etc.) are filtered from `fs_read`/`fs_write` before sandbox profile generation

Policy derived from each script/channel's `[sandbox]` section.

## Prompt Injection Defense

Five-layer defense against prompt injection attacks:

1. **Input Sanitization** (`crates/core/src/sanitize.rs`): Scoring-based injection detection with regex patterns. Flags suspicious content with explicit untrusted markers instead of stripping (preserves legitimate messages). Applied at gateway inbound and tool results.

2. **Context Segregation**: System prompt uses XML structural boundaries (`<system_instructions>`, `<user_memory>`, `<tool_output>`) with trust labels to prevent instruction boundary confusion.

3. **Prompt Hardening**: Security policy injected into system prompt instructing the model to treat external data as data, not instructions. Role boundary enforcement.

4. **Rate Limiting** (`crates/core/src/rate_guard.rs`): Per-session action caps (tool calls, shell commands, file/memory writes, web requests) with warn and block thresholds. Configurable via `[security]` config.

5. **Secret Redaction** (existing): Regex-based redaction of API keys, tokens, and credentials in tool outputs.

## Key Source Files

| File | What |
|------|------|
| `crates/cli/src/main.rs` | Entry point, clap commands, init |
| `crates/cli/src/onboarding.rs` | TUI onboarding wizard (inquire-based) |
| `crates/cli/src/plugins.rs` | Integration catalog, `borg add/remove/plugins` commands |
| `crates/cli/src/repl.rs` | Interactive loop + heartbeat rendering |
| `crates/core/src/agent.rs` | Conversation loop + tool dispatch |
| `crates/core/src/provider.rs` | Provider enum, auto-detection, headers |
| `crates/core/src/llm.rs` | Multi-provider streaming SSE client |
| `crates/core/src/config.rs` | Config parsing with defaults |
| `crates/core/src/identity.rs` | IDENTITY.md load/save |
| `crates/core/src/memory.rs` | Memory loading with token budget |
| `crates/core/src/embeddings.rs` | Embedding API client, cosine similarity, hybrid search merge, embedding cache |
| `crates/core/src/mmr.rs` | MMR diversity re-ranking (Jaccard similarity, greedy selection) |
| `crates/core/src/memory_watcher.rs` | File watcher for auto-re-indexing memory .md files |
| `crates/core/src/session_indexer.rs` | Session transcript indexing for searchable conversations |
| `crates/core/src/chunker.rs` | Markdown-aware content chunking with code fence preservation |
| `crates/core/src/skills.rs` | Skills loading, parsing, progressive token budgeting |
| `crates/core/src/hooks.rs` | Lifecycle hook system (trait, registry, dispatch) |
| `crates/core/src/vitals.rs` | Vitals system: stats, events, decay, drift, VitalsHook |
| `crates/core/src/doctor.rs` | Diagnostic checks and report formatting |
| `crates/core/src/browser.rs` | Chrome detection, CDP session management, browser automation |
| `crates/core/src/claude_cli.rs` | Claude Code CLI subprocess backend: detection, OAuth validation, JSONL streaming |
| `crates/core/src/host_audit.rs` | Host security audit checks (firewall, ports, SSH, permissions, encryption, updates, services) |
| `crates/core/src/git.rs` | Git utilities: ghost commits, git context, turn diff tracking |
| `crates/core/src/project_doc.rs` | Project doc discovery (AGENTS.md / CLAUDE.md) for system prompt |
| `crates/core/src/sanitize.rs` | Prompt injection detection (scoring-based, regex patterns, untrusted content wrapping) |
| `crates/core/src/rate_guard.rs` | Per-session rate limiting for tool calls, shell commands, file/memory writes, web requests |
| `crates/core/src/db.rs` | SQLite database with versioned migrations |
| `crates/core/src/types.rs` | Message (with timestamps), ToolCall, ToolDefinition |
| `crates/heartbeat/src/scheduler.rs` | Pure timer: interval/cron scheduling, quiet hours (timezone-aware), poke signal |
| `crates/sandbox/src/runner.rs` | Script runner: sandboxed subprocess execution |
| `crates/sandbox/src/policy.rs` | SandboxPolicy + command wrapping |
| `crates/sandbox/src/seatbelt.rs` | macOS profile generation |
| `crates/sandbox/src/bubblewrap.rs` | Linux bwrap arg building |
| `crates/apply-patch/src/parser.rs` | Patch DSL parser |
| `crates/apply-patch/src/apply.rs` | Filesystem patch applicator |
| `crates/gateway/src/manifest.rs` | channel.toml parsing |
| `crates/gateway/src/registry.rs` | Scan + register user channels |
| `crates/gateway/src/executor.rs` | Channel script subprocess execution |
| `crates/gateway/src/server.rs` | Axum HTTP server (webhook routes, native Telegram + Slack) |
| `crates/gateway/src/slack/` | Native Slack Bot API integration (types, verify, parse, api) |
| `crates/gateway/src/telegram/` | Native Telegram Bot API integration (types, verify, parse, api, dedup) |
| `crates/gateway/src/twilio/` | Native Twilio integration (WhatsApp + SMS) |
| `crates/gateway/src/handler.rs` | Webhook handler: verify -> parse -> agent -> respond |
| `crates/core/src/integrations/` | Native tool integrations (Gmail, Outlook, Calendar, Notion, Linear) |

## Testing

```sh
cargo test                                               # all tests
cargo test -p borg-apply-patch                           # 13 patch tests
cargo test -p borg-core                                  # config + skills tests
cargo test -p borg-gateway                               # channel manifest + registry tests
cargo test -p borg-plugins                               # catalog + installer tests
```
