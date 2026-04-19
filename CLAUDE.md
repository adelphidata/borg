# Borg

AI personal assistant agent built in Rust. The agent itself is the plugin system — it writes its own tools at runtime. Single binary, all integrations compiled in. Data directory: `~/.borg/`.

## Architecture

Cargo workspace with 7 crates:

| Crate | Purpose |
|-------|---------|
| `cli` | Binary: REPL, TUI, clap commands, onboarding, heartbeat display |
| `core` | Library: agent loop, multi-provider LLM, memory, identity, config, tools, skills, workflows |
| `heartbeat` | Library: proactive scheduler with quiet hours + dedup |
| `sandbox` | Library: macOS Seatbelt + Linux Bubblewrap policies |
| `apply-patch` | Library: patch DSL parser + filesystem applicator |
| `gateway` | Library: webhook gateway, native channel integrations |
| `plugins` | Library: marketplace catalog + plugin installer |

## Build & Test

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy -- -D warnings
```

Binary: `borg`. Requires one of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, a running Ollama instance, or Claude Code CLI at runtime.

Coverage: `just coverage` (HTML) / `just coverage-summary` (text). Target 80%+.

Smaller crates (apply-patch, sandbox, heartbeat, plugins) enforce `#![warn(missing_docs)]`. Add `///` doc comments to new public items.

All integrations compiled unconditionally. iMessage is macOS-only via `#[cfg(target_os = "macos")]`.

## Critical Invariants

### Error Handling — No Silent Swallowing

**NEVER silently discard errors with `let _ = ...` or `.ok()` on operations that matter.** Every error must be logged and either propagated or handled gracefully.

- **Propagate with `?`** when the caller returns `Result` and handles errors upstream (e.g. tool handlers, settings methods).
- **Log + continue** in fire-and-forget contexts (async callbacks, hooks, delivery queues) where propagation would crash a background task.
- **Always `tracing::warn!`** on the error path — even if you fall back to a default, log why.
- Acceptable to use `let _ = ...` ONLY for truly best-effort operations: typing indicators, terminal cleanup in Drop impls, temp file removal.
- If an operation reports success to the user (e.g. "Updated: ..."), it MUST actually succeed or show an error. No lying.

### UX Philosophy — No Approval Prompts

**DO NOT add per-tool-call approval prompts or confirmation dialogs.** The agent just executes.

- Shell commands auto-execute. Only catastrophic commands (rm -rf /, mkfs, dd, curl|sh) are denied.
- No HITL confirmation for tool calls.
- Sandboxing and rate limiting are security boundaries, not approval dialogs.
- If tempted to add "confirm before executing" — don't. Explicitly removed as a design decision.

### Mouse Interaction — Hard Invariant

**Native click+drag text selection MUST work in the transcript with no modifier keys.** Has regressed multiple times — treat as a hard invariant.

**Strategy:** xterm Alternate Scroll Mode (`?1007h`) only, via custom `EnableAlternateScroll` in `crates/cli/src/tui/mod.rs`. Terminal translates wheel events into arrow key sequences. No mouse tracking enabled — click+drag handled by terminal. Reference: `reference/codex/codex-rs/tui/src/tui.rs`.

**Behavior:**
- Click+drag selects text natively (Cmd/Ctrl+C copies). No modifier needed.
- Mouse wheel scrolls transcript one line per tick (terminal wheel→arrow translation).
- PageUp/PageDown scroll 20-line jumps.
- Up/Down navigate composer history (shell-like recall of sent messages). Only exception: if `scroll_offset > 0` (already scrolled up), Up/Down scroll the transcript instead.
- Ctrl+P/Ctrl+N always navigate composer history regardless of scroll state.

**FORBIDDEN (will regress text selection):**
- Escape sequences: `?1000h`, `?1002h`, `?1003h`, `?1006h`
- Crossterm API: `EnableMouseCapture`, `DisableMouseCapture`
- `Event::Mouse` match arms, `App::handle_mouse`, `MouseEventKind` references
- Scrollbar click/drag (requires `?1000h`)

**Guard tests (do not remove or weaken):**
- `crates/cli/src/tui/mod.rs` — escape-sequence correctness + source-level guards that read both `mod.rs` and `app.rs` via `include_str!` and fail if forbidden patterns reintroduced.
- `crates/cli/src/tui/app.rs` — arrow-routing tests (scroll-while-scrolled-up, wheel-from-bottom, active-composer-history), Ctrl+P escape hatch, PageUp/PageDown regression guards.

### Tool Conservation

**Be conservative adding new tools.** Every tool's JSON schema is sent to the LLM every turn (~5KB+ per tool). Prefer adding actions/parameters to existing tools. If achievable via `run_shell` or an existing tool action, don't create a new tool.

### Patch DSL — Every Content Line Needs a Prefix

`+` for added, ` ` (space) for context, `-` for removed. Omitting the prefix is a common mistake.

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

### Security — Blocked Paths

`[security] blocked_paths` defaults: `.ssh`, `.aws`, `.gnupg`, `.config/gh`, `.env`, `credentials`, `private_key`. Filtered from sandbox `fs_read`/`fs_write` before profile generation. `list_dir` checks on every entry.

## Systems Reference

### Tools

| Tool | Purpose |
|------|---------|
| `write_memory` | Write/append to memory files. `scope`: global/local |
| `read_memory` | Read a memory file |
| `memory_search` | Hybrid vector+BM25 search across memory and sessions |
| `list` | List resources (tools, skills, channels, agents) |
| `apply_patch` | Create/update/delete files via patch DSL. Target: cwd, skills, or channels |
| `run_shell` | Execute shell command |
| `read_file` | Read file with line numbers, images, PDFs |
| `list_dir` | List directory (depth max 3, checks blocked paths) |
| `browser` | Headless Chrome automation (requires `browser.enabled`) |
| `web_fetch` | Fetch URL content (requires `web.enabled`) |
| `web_search` | Web search (requires `web.enabled`) |
| `projects` | Manage projects (create/list/get/update/archive/delete) |
| `schedule` | Manage scheduled jobs: prompt tasks, cron commands, workflows |
| `request_user_input` | Request input from user |
| `text_to_speech` | TTS synthesis (requires `tts.enabled`) |
| `generate_image` | Image generation (requires `image_gen.enabled`) |

### Config

DB-only config — modularized at `crates/core/src/config/` (mod.rs, llm.rs, gateway.rs, media.rs, security.rs). All settings stored in SQLite `settings` table as key-value pairs. Complex types (gateway bindings, fallback chains, credentials) stored as JSON.

Two-tier resolution: **Database** → **compiled defaults**. `Config::load_from_db()` builds the runtime Config. `SettingsResolver` provides set/get/unset with validation. No config.toml (V32 migration imports and renames to .bak).

### Memory

Two-tier architecture: short-term (session) + long-term (persistent). All memory stored in SQLite `memory_entries` table — no filesystem memory files.

**Long-term memory** (`memory_entries` table):
- `scope` + `name` uniquely identify an entry (e.g. `global/INDEX`, `global/rust-patterns`, `project:abc/notes`)
- Loaded every turn within token budget; INDEX entry always first, rest by semantic ranking or `updated_at` DESC
- `write_memory` tool writes to DB with injection scanning (prompt override, exfiltration, invisible Unicode patterns rejected)
- Hybrid search: vector (70%) + BM25 (30%) with adaptive weighting, MMR diversity re-ranking, per-term fallback
- Embedding cache with TTL pruning (`last_accessed_at` tracking, 30-day default)
- V34 migration (historical) imported old `~/.borg/MEMORY.md` and `memory/*.md` files into the DB and renamed originals to `.bak`. Filesystem read fallbacks have since been removed — the DB is the only source of truth.

**Short-term memory** (`ShortTermMemory` struct, in-memory):
- Session-scoped working memory accumulating facts from tool calls
- Rendered as `<working_memory>` in system prompt dynamic suffix
- Flushed to daily log entry on session end, consolidated nightly

**Consolidation** (scheduled tasks, seeded in V34):
- Nightly (3 AM): reviews day's sessions, extracts durable info into long-term topic entries
- Weekly (4 AM Sunday): deduplicates, merges, tightens long-term entries; prunes embedding cache

**System prompt tags**: `<long_term_memory trust="stored">` (stable prefix), `<working_memory>` (dynamic suffix)
- Token estimation via `tiktoken-rs` (cl100k_base BPE)

### Skills

Instruction bundles (SKILL.md + YAML frontmatter) teaching CLI tool usage via `run_shell`. Built-in skills embedded via `include_str!` in `crates/core/skills/`: 1password, browser, calendar, database, discord, docker, email, git, github, notes, scheduler, search, skill-creator, slack, weather.

User skills at `~/.borg/skills/<name>/SKILL.md` override built-ins with same name. Progressive loading: metadata (~50 tokens each) always loaded; full body within token budget for available skills. Requirements (bins/env vars) checked at load time against process env and `[credentials]` store. Credentials injected as env vars into `run_shell`.

### Channels & Gateway

**Native integrations** (Rust, no scripts): Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio (WhatsApp/SMS), iMessage (macOS-only).

Script-based channels: `~/.borg/channels/<name>/` with `channel.toml` (runtime, scripts, sandbox, auth).

Gateway bindings (`[[gateway.bindings]]`) provide per-channel/sender LLM routing overrides (provider, model, temperature, memory_scope, identity, activation, thinking).

Sender pairing: unknown senders gated behind approval (`dm_policy`: pairing/open/disabled). Per-channel overrides via `gateway.channel_policies`.

Thread-scoped history: DB session key is `{sender_id}:{thread_id}`. Parsers populate `thread_id` from platform-native identifiers (Slack: `thread_ts`, Teams: `reply_to_id`, Discord: `channel_id`, Google Chat: `thread.name`, Telegram: `message_thread_id`).

### Collaboration Modes

Three modes (config, `/mode` TUI, `--mode` CLI): **Default** (asks questions), **Execute** (autonomous), **Plan** (read-only, blocks mutating tools, produces `<proposed_plan>`). Templates in `crates/core/templates/collaboration_mode/`.

`/plan` shortcut toggles Plan mode with auto-restore on proceed. `App::previous_collab_mode` is the single source of truth for the transient Plan→execute flow.

### Agent Loop

`core/agent.rs` — streams LLM response, parses tool calls, executes, loops until text-only. `<internal>` tags stripped in real-time. Messages persisted to SQLite immediately (crash recovery). System prompt assembled each turn: IDENTITY.md + time + git context + mode + memory + project docs + coding instructions + skills.

### Workflows

Durable multi-step orchestration. LLM decomposes into ordered steps; each runs as isolated agent turn with prior outputs injected. Persists in SQLite — survives crashes/restarts. All Claude models skip workflows (`workflow.enabled = "auto"`). Key files: `crates/core/src/workflow/`, `crates/core/src/db/workflow.rs`.

### Heartbeat

Proactive check-ins. Interval (default 30m) or cron, quiet hours (00:00–06:00). `~/.borg/HEARTBEAT.md` checklist injected. Channel delivery honors gateway bindings. Suppresses empty/duplicate/ack-only responses. Poke: `borg poke` / `/poke` triggers immediate heartbeat.

### Database

SQLite at `~/.borg/borg.db`. V31 migrations run on `Database::open()`. Key tables: sessions, messages, scheduled_tasks, task_runs, channel_sessions, channel_messages, settings, pairing_requests, approved_senders, embedding_cache, vitals_events, projects, workflows, workflow_steps.

### Sandboxing

macOS: `sandbox-exec` with Seatbelt profiles (deny-all default). Linux: `bwrap` with namespace isolation. Policy from `[sandbox]` section per script/channel. Blocked paths filtered before profile generation.

### Vitals

Event-sourced agent health: 5 stats (stability, focus, sync, growth, happiness) computed by replaying HMAC-verified events. `VitalsHook` listens on SessionStart, BeforeAgentStart, AfterToolCall. `borg status` / `/status`.

### Lifecycle Hooks

Two layers share one `Hook` trait / `HookRegistry` / 9 `HookPoint` variants in `crates/core/src/hooks.rs`:

- **Compiled-in hooks** — `VitalsHook`, `ActivityHook`, `BondHook`, `EvolutionHook` registered in `crates/cli/src/repl.rs` and `crates/cli/src/tui/mod.rs`.
- **User script hooks** — `ScriptHook` loaded from `~/.borg/hooks.json` (Claude Code / codex schema). Events: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`. Runs `sh -c <command>` with a JSON payload as `$1`, bounded by `timeout` (default 60s, clamped to `[1,600]`). `PreToolUse` non-zero exit / timeout returns `HookAction::Skip` and aborts the tool call. All other failure modes (parse error, spawn failure, non-zero exit, panic) log a `tracing::warn!` and return `Continue` — **hooks can never break the agent**. Gated on `hooks.enabled` (default true). See `docs/hooks.md`.

### Prompt Injection Defense

5 layers: input sanitization (scoring-based, flags rather than strips), context segregation (XML trust boundaries), prompt hardening, rate limiting (per-session action caps), secret redaction.

### Git Utilities

`crates/core/src/git.rs` — ghost commits (snapshot repo on session start using temp index, enables atomic undo), git context for system prompt.

### Debugging

Logs at `~/.borg/logs/`: `tui.log` (TUI session), `daemon.log`/`daemon.err` (daemon), `{date}.jsonl` (structured). Check `tui.log` for LLM errors, tool failures, and warnings. Config at `~/.borg/config.toml`. Enable verbose logging with `[debug]` section. `RUST_LOG=debug` env var for module-level control.

## Adding New Things

### New Setting (3 touch points)

1. **`crates/core/src/config/mod.rs`** — Add field to config struct + `apply_setting()` match arm
2. **`crates/core/src/settings.rs`** — Add entry to `SETTING_REGISTRY` (key + extractor fn)
3. **`crates/cli/src/tui/settings_popup.rs`** — Add `SettingEntry` to `SETTINGS` array (key, label, kind, category)

`SettingKind` options: Bool (Space toggles), Float (arrows ±0.1), Uint (Enter to edit), Select (Left/Right cycle).

### New Tool

Strongly prefer extending an existing tool with a new action parameter. If you must add:
- Definition in `crates/core/src/tool_definitions.rs`
- Handler in `crates/core/src/tool_handlers/<name>.rs`
- Group mapping in `crates/core/src/tool_catalog.rs` (`tool_group()`)
- Plan mode: new tools default to blocked — add to allowlist in agent.rs if non-mutating

### New Skill

Add `crates/core/skills/<name>/SKILL.md` with YAML frontmatter (name, description, requires.bins, requires.env). Register `include_str!` in `crates/core/src/skills.rs`.

### New Native Channel

Add directory `crates/gateway/src/<name>/` with types, parse, api, verify modules. Register in `crates/gateway/src/server.rs` routes and `crates/gateway/src/channel_init.rs`.
