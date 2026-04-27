# Borg

AI personal assistant agent in Rust. The agent writes its own tools at runtime. Single binary, all integrations compiled in. Data dir: `~/.borg/`.

## Architecture

Cargo workspace, no-prefix crate names:

| Crate | Purpose |
|-------|---------|
| `interface` | Binary `borg`: REPL, TUI, clap, onboarding |
| `cortex` | Agent loop, LLM, memory, identity, config, tools, skills, workflows |
| `heartbeat` | Proactive scheduler |
| `sandbox` | macOS Seatbelt + Linux Bubblewrap |
| `coding` | Patch DSL parser + applicator |
| `gateway` | Webhook gateway, native channel integrations (daemon-internal sub-library) |
| `plugins` | Marketplace catalog + installer |
| `protocols` | tonic gRPC schema |
| `identity` | Internal CA, cert lifecycle, OS keychain wrapping |
| `borgd` | Long-running gRPC daemon |

## Build & Test

```sh
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log
```

**Run tests once.** `tee` to a log, then grep/tail. Use `--lib --bins` for feedback; full `--workspace` only for ship checks. Coverage: `just coverage` / `just coverage-summary`. Target 80%+.

Requires one of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, Ollama, or Claude Code CLI.

Smaller crates (apply-patch, sandbox, heartbeat, plugins) enforce `#![warn(missing_docs)]`. iMessage is macOS-only via `#[cfg(target_os = "macos")]`.

## Critical Invariants

### Error Handling — No Silent Swallowing

**NEVER silently discard errors with `let _ = ...` or `.ok()` on operations that matter.**

- Propagate with `?` when caller returns `Result`.
- Log + continue in fire-and-forget contexts; always `tracing::warn!` on the error path.
- `let _ = ...` ONLY for truly best-effort ops (typing indicators, terminal cleanup in Drop, temp file removal).
- If we report success to the user, it MUST actually succeed.

### UX — No Approval Prompts

**The agent just executes.** No per-tool-call approval prompts or confirmation dialogs. Shell commands auto-execute; only catastrophic commands (rm -rf /, mkfs, dd, curl|sh) are denied at the sandbox layer. Sandboxing and rate limiting are security boundaries, not approval dialogs. Explicitly removed as a design decision.

### Mouse Interaction — Hard Invariant

**Native click+drag text selection MUST work in the transcript with no modifier keys.** Has regressed multiple times.

**Strategy:** xterm Alternate Scroll Mode (`?1007h`) only, via custom `EnableAlternateScroll` in `crates/interface/src/tui/mod.rs`. Terminal translates wheel events into arrow keys. No mouse tracking enabled.

**Behavior:** Click+drag selects text natively. Wheel scrolls one line/tick. PageUp/Down = 20-line jumps. Up/Down navigate composer history (exception: when `scroll_offset > 0`, they scroll transcript). Ctrl+P/N always navigate history.

**FORBIDDEN (will regress text selection):**
- Escape sequences: `?1000h`, `?1002h`, `?1003h`, `?1006h`
- Crossterm: `EnableMouseCapture`, `DisableMouseCapture`, `Event::Mouse`, `App::handle_mouse`, `MouseEventKind`
- Scrollbar click/drag (requires `?1000h`)

Guard tests in `crates/interface/src/tui/mod.rs` and `app.rs` use `include_str!` to detect reintroduced patterns. Do not weaken.

### Tool Conservation

Every tool's JSON schema is sent to the LLM every turn (~5KB+). Prefer adding actions/parameters to existing tools. If achievable via `run_shell` or an existing tool, don't create a new one.

### Test Quality — No Fluff Tests

**Every test must be able to fail for a real-world reason.** Reject:

- **Tautological** — asserting on values the test set, on `Default`/`Clone` output, or compile-time constants.
- **Over-mocked** — mock returns canned data; test asserts on canned data. Production path isn't exercised.
- **Smoke-only** — `.is_ok()` / `.is_some()` / "didn't panic" with no meaningful assertion.
- **Enum-variant-exists** — `assert!(matches!(x, Event::Foo { .. }))` is compiler-checked.
- **Source-grep tests** — only acceptable for invariants types can't express (the prompt-cache and TUI mouse guards are exceptions, not templates).
- **Near-duplicate per-variant tests** — collapse into table-driven.

**Every commit/PR that changes behavior must include tests.** Bug fixes need a regression test. Document non-obvious regression guards with the invariant they protect.

### Patch DSL

`+` added, ` ` (space) context, `-` removed. Omitting the prefix is a common mistake.

```
*** Begin Patch
*** Add File: path/to/new.py
+import sys
+print("hello")
*** Update File: path/to/file.py
@@
 context
-old
+new
*** Delete File: path/to/old.py
*** End Patch
```

### Security — Blocked Paths

`[security] blocked_paths` defaults: `.ssh`, `.aws`, `.gnupg`, `.config/gh`, `.env`, `credentials`, `private_key`. Filtered from `fs_read`/`fs_write` and checked in `list_dir`.

## Systems Reference

### Tools

`write_memory`, `read_memory`, `memory_search`, `list`, `apply_patch`, `run_shell`, `read_file`, `list_dir`, `browser`, `web_fetch`, `web_search`, `projects`, `schedule`, `request_user_input`, `text_to_speech`, `generate_image`. Definitions in `crates/cortex/src/tool_definitions.rs`.

### Config

DB-only — `crates/cortex/src/config/`. All settings in SQLite `settings` table. Two-tier: DB → compiled defaults. `Config::load_from_db()` builds runtime; `SettingsResolver` for set/get/unset. No config.toml.

### Memory

Two-tier: short-term (session, in-memory) + long-term (`memory_entries` table). DB is sole source of truth.

- `scope` + `name` keys (e.g. `global/INDEX`, `project:abc/notes`). INDEX always loaded first.
- `write_memory` scans for prompt injection (override, exfil, invisible Unicode) and enforces 20k-token cap (warn at 8k).
- Hybrid search: vector 70% + BM25 30%, MMR re-ranking, embedding cache with TTL pruning.
- Consolidation tasks: nightly 3 AM extracts to long-term; weekly 4 AM Sunday dedupes.
- System prompt: `<long_term_memory trust="stored">` (stable prefix), `<working_memory>` (dynamic suffix). Token est. via `tiktoken-rs`.

### Skills

SKILL.md + YAML frontmatter teaching CLI tool usage via `run_shell`. Built-ins embedded via `include_str!` in `crates/cortex/skills/`. User skills at `~/.borg/skills/<name>/` override built-ins. Progressive loading: metadata always; full body within budget. Requirements (bins/env) checked at load. Credentials injected as env vars into `run_shell`.

### Channels & Gateway

Native (Rust): Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage. Script channels at `~/.borg/channels/<name>/` with `channel.toml`.

Per-channel/sender LLM routing via `[[gateway.bindings]]`. Sender pairing via `dm_policy` (pairing/open/disabled), per-channel overrides via `gateway.channel_policies`. Thread-scoped history: session key `{sender_id}:{thread_id}`.

### Collaboration Modes

**Default** (asks), **Execute** (autonomous), **Plan** (read-only, blocks mutating tools). `/plan` toggles with auto-restore. Templates in `crates/cortex/templates/collaboration_mode/`.

### Agent Loop

`core/agent.rs` — streams LLM, parses tool calls, executes, loops until text-only. `<internal>` tags stripped in real-time. Messages persisted to SQLite immediately. System prompt assembled per turn from IDENTITY + time + git + mode + memory + project docs + skills.

### Workflows

Multi-step orchestration. LLM decomposes into ordered steps; each runs as isolated agent turn. Persists in SQLite. Claude models skip workflows. `crates/cortex/src/workflow/`, `crates/cortex/src/db/workflow.rs`.

### Heartbeat

Proactive check-ins (default 30m or cron, quiet 00:00–06:00). `~/.borg/HEARTBEAT.md` checklist. Honors gateway bindings. Suppresses empty/duplicate/ack-only. Trigger: `borg poke` / `/poke`.

### Database

SQLite at `~/.borg/borg.db`. Migrations run on `Database::open()`. Tables: sessions, messages, scheduled_tasks, channel_*, settings, pairing_requests, approved_senders, embedding_cache, vitals_events, projects, workflows.

### Sandboxing

macOS: `sandbox-exec` Seatbelt (deny-all default). Linux: `bwrap` namespace isolation. Policy from `[sandbox]` per script/channel. Blocked paths filtered before profile generation.

### Vitals & Posture

5 stats (stability, focus, sync, growth, happiness) computed by replaying HMAC-verified events. `VitalsHook` on SessionStart/BeforeAgentStart/AfterToolCall. `borg status` / `/status`.

Posture v2: 3 states with XP multiplier — `Reduced` ×0.85, `Default` ×1.00, `Boosted` ×1.10. Transitions emit `security_posture_changed` into hash chain (rate-limited 2/hr). Replay reads from chain only, not live config.

### Lifecycle Hooks

One `Hook` trait / `HookRegistry` / 9 `HookPoint` variants in `crates/cortex/src/hooks.rs`. Compiled-in: `VitalsHook`, `ActivityHook`, `BondHook`, `EvolutionHook`. User scripts at `~/.borg/hooks.json` (Claude Code-compatible schema). `PreToolUse` non-zero exit aborts the tool call. All other failures log + continue — **hooks can never break the agent**. See `docs/hooks.md`.

### Self-Healing

Daily maintenance task fires 02:00 — prunes logs, evicts stale embeddings, heals stalled tasks, surfaces persistent warnings. No LLM call. Daemon scans every 5min for tasks with `next_run` drift > 1h. `write_memory` 20k-token cap enforced. `skill_audit` table catches user SKILL.md tampering. `/heal` invokes on demand. See `docs/self-healing.md`.

### Prompt Injection Defense

5 layers: input sanitization (scoring), context segregation (XML trust boundaries), prompt hardening, rate limiting, secret redaction.

### Git Utilities

`crates/cortex/src/git.rs` — ghost commits (snapshot via temp index, atomic undo), git context for system prompt.

### Debugging

Logs at `~/.borg/logs/`: `tui.log`, `daemon.log`/`daemon.err`, `{date}.jsonl`. Config at `~/.borg/config.toml`. `RUST_LOG=debug` for module-level control.

## Adding New Things

### New Setting

1. Add Rust field + default to matching struct in `crates/cortex/src/config/<section>.rs`.
2. Add line to `define_settings!` in `crates/cortex/src/config/settings_table.rs`. Macro generates registry, `apply_setting()` arm, and TUI popup row.

To show in `/settings` popup, add to `tui_settings { … }` block: `"my.setting" => "Label", Bool, "Category";`. Kinds: `Bool`, `Float`, `Uint`, `Select`.

### New Tool

Strongly prefer extending an existing tool. If you must add:
- Definition: `crates/cortex/src/tool_definitions.rs`
- Handler: `crates/cortex/src/tool_handlers/<name>.rs`
- Group: `crates/cortex/src/tool_catalog.rs` (`tool_group()`)
- Plan mode: defaults to blocked — add to allowlist in `agent.rs` if non-mutating

### New Skill

`crates/cortex/skills/<name>/SKILL.md` with YAML frontmatter (name, description, requires.bins, requires.env). Register `include_str!` in `crates/cortex/src/skills.rs`.

### New Native Channel

`crates/gateway/src/<name>/` with types/parse/api/verify modules. Register in `crates/gateway/src/server.rs` and `channel_init.rs`.
