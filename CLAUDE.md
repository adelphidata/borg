# Borg

AI personal assistant agent built in Rust. The agent itself is the plugin system — it writes its own tools at runtime. Single binary, all integrations compiled in. Data directory: `~/.borg/`.

> **Active migration in progress.** Borg is moving from a single-binary in-process REPL/TUI agent to a daemonized platform (`borgd` + gRPC + mTLS-paired clients + Layer 5 ingress adapters). See "Redesign In Progress (v1: gRPC daemon + mTLS)" at the bottom of this file, and the full spec at `docs/redesign/`. Sections above describe shipped behavior that still works during the migration; prefer the new architecture when adding code that touches the agent loop, transport, or ingress.

## Architecture

Cargo workspace. Crate names follow a no-prefix convention; several are themed around the cybernetic-body metaphor:

| Crate | Purpose |
|-------|---------|
| `interface` | Binary: REPL, TUI, clap commands, onboarding, heartbeat display. Binary name remains `borg`. |
| `cortex` | Library: agent loop, multi-provider LLM, memory, identity, config, tools, skills, workflows |
| `heartbeat` | Library: proactive scheduler with quiet hours + dedup |
| `sandbox` | Library: macOS Seatbelt + Linux Bubblewrap policies |
| `coding` | Library: patch DSL parser + filesystem applicator |
| `gateway` | Library: webhook gateway, native channel integrations |
| `plugins` | Library: marketplace catalog + plugin installer |
| `protocols` | Library: tonic gRPC schema for the daemon and its clients |
| `identity` | Library: internal CA, cert lifecycle, OS keychain wrapping (PKI) |
| `borgd` | Binary: long-running gRPC daemon (Unix daemon convention preserved) |

## Build & Test

```sh
cargo build
cargo test
cargo fmt --check
cargo clippy -- -D warnings
```

**Run tests once.** `tee` to a log, then grep/tail it — don't re-run for each view. Skip `--no-run` pre-compiles. Use `--lib --bins` for feedback; full `--workspace` only for ship checks.

```sh
cargo test --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log
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

**Strategy:** xterm Alternate Scroll Mode (`?1007h`) only, via custom `EnableAlternateScroll` in `crates/interface/src/tui/mod.rs`. Terminal translates wheel events into arrow key sequences. No mouse tracking enabled — click+drag handled by terminal. Reference: `reference/codex/codex-rs/tui/src/tui.rs`.

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
- `crates/interface/src/tui/mod.rs` — escape-sequence correctness + source-level guards that read both `mod.rs` and `app.rs` via `include_str!` and fail if forbidden patterns reintroduced.
- `crates/interface/src/tui/app.rs` — arrow-routing tests (scroll-while-scrolled-up, wheel-from-bottom, active-composer-history), Ctrl+P escape hatch, PageUp/PageDown regression guards.

### Tool Conservation

**Be conservative adding new tools.** Every tool's JSON schema is sent to the LLM every turn (~5KB+ per tool). Prefer adding actions/parameters to existing tools. If achievable via `run_shell` or an existing tool action, don't create a new tool.

### Test Quality — No Fluff Tests

**Every test must be able to fail for a real-world reason.** A passing test with no failure mode is noise — it inflates the suite, slows CI, and provides false confidence. When writing or reviewing tests, reject these patterns:

- **Tautological** — asserting on values the test itself just set, on `Default`/`Clone`/auto-derived trait output, or on compile-time constants. If the test and the code under test are both just reading the same literal, delete the test.
- **Over-mocked** — a mock/stub returns canned data and the test asserts on that canned data. The production code path isn't exercised; the mock is. Either stand up a real fixture or delete the test.
- **Smoke-only** — `.is_ok()` / `.is_some()` / "didn't panic" with no meaningful assertion on the result. Either add a real assertion or delete.
- **Enum-variant-exists** — `let x = Event::Foo {}; assert!(matches!(x, Event::Foo { .. }));` is already compiler-checked. Delete.
- **Source-grep tests** — `include_str!("mod.rs"); assert!(src.contains("fn foo"))` is brittle and re-asserts what the compiler already enforces. Only acceptable for documented invariants that can't be expressed in types (see the `build_system_prompt_*` prompt-cache guards and the TUI `include_str!` forbidden-pattern guards — these are the exceptions, not a template).
- **Near-duplicate per-variant tests** — one test per enum variant / per config value / per provider, all with the same body. Collapse into a single table-driven test.

**What a good test looks like:** exercises a real code path, sets up inputs that could realistically occur, asserts on a specific observable outcome, and would fail if the production logic broke. Prefer fewer strong tests to many weak ones.

**Every commit / PR that changes behavior must include tests.** Bug fixes need a regression test that fails before the fix and passes after. New features need tests for the happy path plus at least one edge case. The only exemptions are pure doc/comment/formatting changes and dependency bumps with no behavior change — anything else ships with tests.

**When a regression guard looks like a fluff test, document why.** If a test is intentionally checking a constant or a source-level pattern because it enforces an invariant that types can't express (prompt cache stability, forbidden mouse-capture APIs, etc.), add a comment naming the invariant and the incident that motivated the guard. Otherwise reviewers will correctly delete it.

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

DB-only config — modularized at `crates/cortex/src/config/` (mod.rs, llm.rs, gateway.rs, media.rs, security.rs). All settings stored in SQLite `settings` table as key-value pairs. Complex types (gateway bindings, fallback chains, credentials) stored as JSON.

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

Instruction bundles (SKILL.md + YAML frontmatter) teaching CLI tool usage via `run_shell`. Built-in skills embedded via `include_str!` in `crates/cortex/skills/`: 1password, browser, calendar, database, discord, docker, email, git, github, notes, scheduler, search, skill-creator, slack, weather.

User skills at `~/.borg/skills/<name>/SKILL.md` override built-ins with same name. Progressive loading: metadata (~50 tokens each) always loaded; full body within token budget for available skills. Requirements (bins/env vars) checked at load time against process env and `[credentials]` store. Credentials injected as env vars into `run_shell`.

### Channels & Gateway

**Native integrations** (Rust, no scripts): Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio (WhatsApp/SMS), iMessage (macOS-only).

Script-based channels: `~/.borg/channels/<name>/` with `channel.toml` (runtime, scripts, sandbox, auth).

Gateway bindings (`[[gateway.bindings]]`) provide per-channel/sender LLM routing overrides (provider, model, temperature, memory_scope, identity, activation, thinking).

Sender pairing: unknown senders gated behind approval (`dm_policy`: pairing/open/disabled). Per-channel overrides via `gateway.channel_policies`.

Thread-scoped history: DB session key is `{sender_id}:{thread_id}`. Parsers populate `thread_id` from platform-native identifiers (Slack: `thread_ts`, Teams: `reply_to_id`, Discord: `channel_id`, Google Chat: `thread.name`, Telegram: `message_thread_id`).

### Collaboration Modes

Three modes (config, `/mode` TUI, `--mode` CLI): **Default** (asks questions), **Execute** (autonomous), **Plan** (read-only, blocks mutating tools, produces `<proposed_plan>`). Templates in `crates/cortex/templates/collaboration_mode/`.

`/plan` shortcut toggles Plan mode with auto-restore on proceed. `App::previous_collab_mode` is the single source of truth for the transient Plan→execute flow.

### Agent Loop

`core/agent.rs` — streams LLM response, parses tool calls, executes, loops until text-only. `<internal>` tags stripped in real-time. Messages persisted to SQLite immediately (crash recovery). System prompt assembled each turn: IDENTITY.md + time + git context + mode + memory + project docs + coding instructions + skills.

### Workflows

Durable multi-step orchestration. LLM decomposes into ordered steps; each runs as isolated agent turn with prior outputs injected. Persists in SQLite — survives crashes/restarts. All Claude models skip workflows (`workflow.enabled = "auto"`). Key files: `crates/cortex/src/workflow/`, `crates/cortex/src/db/workflow.rs`.

### Heartbeat

Proactive check-ins. Interval (default 30m) or cron, quiet hours (00:00–06:00). `~/.borg/HEARTBEAT.md` checklist injected. Channel delivery honors gateway bindings. Suppresses empty/duplicate/ack-only responses. Poke: `borg poke` / `/poke` triggers immediate heartbeat.

### Database

SQLite at `~/.borg/borg.db`. V31 migrations run on `Database::open()`. Key tables: sessions, messages, scheduled_tasks, task_runs, channel_sessions, channel_messages, settings, pairing_requests, approved_senders, embedding_cache, vitals_events, projects, workflows, workflow_steps.

### Sandboxing

macOS: `sandbox-exec` with Seatbelt profiles (deny-all default). Linux: `bwrap` with namespace isolation. Policy from `[sandbox]` section per script/channel. Blocked paths filtered before profile generation.

### Vitals

Event-sourced agent health: 5 stats (stability, focus, sync, growth, happiness) computed by replaying HMAC-verified events. `VitalsHook` listens on SessionStart, BeforeAgentStart, AfterToolCall. `borg status` / `/status`.

### Lifecycle Hooks

Two layers share one `Hook` trait / `HookRegistry` / 9 `HookPoint` variants in `crates/cortex/src/hooks.rs`:

- **Compiled-in hooks** — `VitalsHook`, `ActivityHook`, `BondHook`, `EvolutionHook` registered in `crates/interface/src/repl.rs` and `crates/interface/src/tui/mod.rs`.
- **User script hooks** — `ScriptHook` loaded from `~/.borg/hooks.json` (Claude Code / codex schema). Events: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`. Runs `sh -c <command>` with a JSON payload as `$1`, bounded by `timeout` (default 60s, clamped to `[1,600]`). `PreToolUse` non-zero exit / timeout returns `HookAction::Skip` and aborts the tool call. All other failure modes (parse error, spawn failure, non-zero exit, panic) log a `tracing::warn!` and return `Continue` — **hooks can never break the agent**. Gated on `hooks.enabled` (default true). See `docs/hooks.md`.

### Self-Healing

Daily maintenance task (`task_type = 'maintenance'`, seeded in V37, fires 02:00) runs a headless doctor sweep, prunes `~/.borg/logs/*.jsonl` and `activity_log` past retention, evicts stale embeddings, heals stalled scheduled tasks, and surfaces warnings that persist across two consecutive runs. No LLM call — dispatcher calls `cortex::maintenance::run_daily_maintenance` directly, so it works with no provider configured. Daemon loop also scans every 5 min for scheduled tasks whose `next_run` drifted > `STALLED_TASK_GRACE_SECS` (1h) into the past, records `task_runs.status='missed'`, resets `next_run`. `write_memory` enforces a hard 20k-token cap (warn at 8k) so oversized entries fail loud instead of silently getting dropped by the token-budget loader. V39 added a `skill_audit` table (SHA-256 of every user `SKILL.md`) so the doctor Skills check flags post-install tampering on subsequent loads. On TUI open, `App::new` surfaces any `persistent_warnings` from the latest `doctor_runs` row as a one-shot `System` cell after the opening card — intentionally not a pinned banner. `/heal` slash command invokes the same `run_daily_maintenance` on demand (sync call from `cmd_heal` in `crates/interface/src/tui/commands.rs`). See `docs/self-healing.md`.

### Prompt Injection Defense

5 layers: input sanitization (scoring-based, flags rather than strips), context segregation (XML trust boundaries), prompt hardening, rate limiting (per-session action caps), secret redaction.

### Git Utilities

`crates/cortex/src/git.rs` — ghost commits (snapshot repo on session start using temp index, enables atomic undo), git context for system prompt.

### Debugging

Logs at `~/.borg/logs/`: `tui.log` (TUI session), `daemon.log`/`daemon.err` (daemon), `{date}.jsonl` (structured). Check `tui.log` for LLM errors, tool failures, and warnings. Config at `~/.borg/config.toml`. Enable verbose logging with `[debug]` section. `RUST_LOG=debug` env var for module-level control.

## Adding New Things

### New Setting (single table)

1. **`crates/cortex/src/config/<section>.rs`** — Add the Rust field + default to the matching config struct.
2. **`crates/cortex/src/config/settings_table.rs`** — Add one line to the `define_settings!` macro. The macro generates `SETTING_REGISTRY`, the `apply_setting()` match arm, and (when you include it in the `tui_settings` block) the `/settings` popup row.

To show the setting in the TUI popup, append an entry to the `tui_settings { … }` block at the bottom of `settings_table.rs`:

```rust
"my.new.setting" => "Display Label", Bool, "Category";
```

`TuiSettingKind` options: `Bool` (Space toggles), `Float` (arrows ±0.1), `Uint` (Enter to edit), `Select` (Left/Right cycle). A unit test asserts every `tui_settings` key resolves in `SETTING_REGISTRY`.

### New Tool

Strongly prefer extending an existing tool with a new action parameter. If you must add:
- Definition in `crates/cortex/src/tool_definitions.rs`
- Handler in `crates/cortex/src/tool_handlers/<name>.rs`
- Group mapping in `crates/cortex/src/tool_catalog.rs` (`tool_group()`)
- Plan mode: new tools default to blocked — add to allowlist in agent.rs if non-mutating

### New Skill

Add `crates/cortex/skills/<name>/SKILL.md` with YAML frontmatter (name, description, requires.bins, requires.env). Register `include_str!` in `crates/cortex/src/skills.rs`.

### New Native Channel

Add directory `crates/gateway/src/<name>/` with types, parse, api, verify modules. Register in `crates/gateway/src/server.rs` routes and `crates/gateway/src/channel_init.rs`.

> Note: under the v1 redesign, native channels move into `borgd` as Layer 5 ingress adapters. New channels added now should be structured to port cleanly into the daemon — see Task 03 below.

## Redesign In Progress (v1: gRPC daemon + mTLS)

**Status:** Task 01 (daemon kernel + proto + DaemonClient + TUI/gateway migration) is complete; one follow-up — wiring `CapabilityRouter` into runtime tool dispatch (see `crates/borgd/src/session/executor.rs` FIXME) — remains. Tasks 02–05 not started. Spec at `docs/redesign/`.

**When in doubt, read the spec:** `docs/redesign/architecture-redesign.md` (overall), `auth-and-onboarding.md` (mTLS + PAKE), `approvals.md` (tier system), `llm-providers.md` (provider trait + budgets), `data-handling.md` (retention + backups), `security-posture.md` (3-state posture + XP multiplier), `threat-model.md`, `mobile.md`, plus per-task plans `tasks/01-daemon-grpc-kernel.md` … `tasks/05-posture-data-cleanup.md`.

### Architecture Shift

**From:** Single `borg` binary running REPL/TUI in-process; agent loop runs inside CLI process.

**To:** Headless daemon (`borgd`) owns the agent loop, scheduled tasks, heartbeat, hooks, all ingress adapters, and is the **single SQLite writer**. Multiple clients connect as peers over gRPC: TUI, OpenAI-compat HTTP, native channels, MCP server, future mobile.

**Preserved invariants:** "The agent just executes" (no per-tool-call approval prompts). Catastrophic-command denylist at sandbox layer. Mouse-selection rules in TUI. Evolution gameplay (XP, vitals, heartbeat) untouched in semantics — wiring moves to daemon.

### New & Modified Crates

| Crate | Status | Purpose |
|-------|--------|---------|
| `crates/borgd` | **new, partial** | Daemon binary. Async tokio runtime, flock single-instance, UDS gRPC listener. Owns agent loop (lifted from `crates/cortex/src/agent.rs`), `SessionHost` per-session broadcast, `PromptRegistry`, scheduled tasks, heartbeat, hooks, all ingress. |
| `crates/protocols` | **new, evolving** | tonic gRPC schema. Services: `Session`, `Status`, `Capability`, `Admin`. Codegen for Rust now; future TS/Swift/Kotlin for v2 mobile. |
| `crates/identity` | not started | Internal CA: issue/renew/revoke certs, OS-keychain wrapping (macOS Keychain / Linux Secret Service or TPM / Windows DPAPI). Private keys never on disk. |
| `crates/interface` | **migrating** | TUI rewriting from in-process agent to gRPC client. New `daemon_client/` submodule with ergonomic wrappers (`send_message_with_images`, `get_state`, `compact`, `undo`, `rewind`, admin RPCs). `crates/interface/src/service/` and `in_flight.rs` will be deleted. |
| `crates/gateway` | **migrating** | 8 native channel adapters port into `borgd`. Crate will downgrade to thin re-exports or be removed once Task 03 lands. |
| `crates/cortex` | **library only, no breaking API** | Tests still link it; both `borgd` and `cli` link it. Agent loop logic only invoked by `borgd` post-migration. Do not add new in-process orchestration here. |

### Transport: gRPC over HTTP/2

- Default bind: Unix domain socket `$BORG_HOME/borgd.sock` (loopback mode).
- `--listen 127.0.0.1:PORT` opt-in for loopback TCP.
- Services (`crates/protocols/proto/`):
  - `Session` — `Open`, `Send` (multimodal: text + `Vec<InlineImage>`), `Stream` (resumable token deltas + tool events).
  - `Status` — `Vitals`, `Posture`, `Budget`, `Heartbeat`.
  - `Capability` — bidi-stream; clients advertise capabilities, daemon dispatches invocations.
  - `Admin` — `Pair`, pairing/approval requests, sessions, scheduled tasks, projects, agent control (compact/undo/rewind), popups.
- **Resumable event log** is the central primitive: every turn's events (token deltas, tool-call start/result, capability invocations, audit entries, memory writes, system events) get a monotonic per-session cursor. Clients subscribe with last-seen cursor; daemon replays missed events from `SessionHost` ring buffer on reconnect.

### Auth & Pairing (Task 02)

mTLS with daemon-internal CA. **No PEM files for users — pairing is PAKE-based with a short code.**

- Daemon on first start: generates self-signed root CA (Ed25519, 5y) + server cert (90d, auto-renewed at 14d remaining). CA private key wrapped by OS keychain.
- `borg pair --host <daemon>`: daemon shows 8-char base32 code + QR (5-min TTL, rate-limited). Client and daemon run SPAKE2/CPace PAKE handshake; within that channel the client generates an Ed25519 keypair and sends a self-signed cert + device identity. Daemon stores `(device_id, fingerprint, scope, paired_at)` in `paired_devices`.
- Subsequent connections: standard mTLS with pinned-fingerprint verification on both sides. No PAKE replay.
- Silent renewal at 14d-to-expiry via `/internal/renew`.
- **Verifiers (v1):** `TofuVerifier` (default — first connection pins) and `TailscaleVerifier` (opt-in — uses `tailscale whois`).

**Scope tiers (audit categorization, NOT prompts — see Approvals below):**
- `Read` (T0): `read_file`, `list_dir`, `web_fetch`, `memory_search`.
- `Write` (T1): `apply_patch`, `write_memory`.
- `Execute` (T2): `run_shell` in sandbox.
- `Privileged` (T3): host fs outside `BORG_HOME`, network egress beyond approved hosts, credential mutation.

Listener has `max_tier` ceiling. Even a `Privileged` device is capped by the listener it connected through. Channel senders never grant `Privileged`.

### Capability Protocol & Layer 5 Ingress (Task 03)

Clients advertise capabilities on connect; daemon's tool registry routes capability-tools to best-available client, falling back to daemon-side execution.

**v1 capabilities:** `terminal.exec`, `file.pick`, `clipboard.read`, `clipboard.write` (TUI/future web); `host.run_shell` (daemon-side, sandboxed — default for `run_shell`); `host.read_file` (daemon-side).

**Layer 5 adapters (all live in `borgd`):**
1. **Native channels (8):** Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage (macOS-only). Ported from `crates/gateway`. Webhook-signature auth, sender-approval flow, per-channel tier ceiling preserved.
2. **OpenAI-compat HTTP:** `/v1/chat/completions` (stream + non-stream), `GET /v1/models`. Bearer token (mapped to paired-device cert) or direct mTLS. Unlocks ChatBox, Open WebUI, LobeChat, official OpenAI SDKs.
3. **Generic webhook:** `POST /v1/webhook/{name}`, HMAC-SHA256, user-provided JSONPath for `sender_id`/`thread_id`/`body`.
4. **MCP server:** Read-only tools (`memory_search`, `read_memory`, `web_fetch`, `web_search`) over stdio + HTTP.

### LLM Providers & Budgets (Task 04)

- Unified `Provider` trait. Compiled-in: OpenAI, Anthropic, Gemini, Groq, DeepSeek, OpenRouter, Ollama, Claude Code CLI, BYO-endpoint.
- Categories: `PublicCloud`, `Local`, `BYOEndpoint`.
- **Versioned `provider_rate_cards`** keyed by `(provider_id, model, effective_at)`. Bundled with binary; opt-in refresh via `borg providers refresh-rates` against signed manifest. **Never auto-fetch.** Replay computes historical USD with the rate card active at the time.
- Pre-flight: `count_tokens` estimate + USD via rate card. Post-flight: reconcile with provider's actual token counts.
- Budgets v1: single global `default` scope (soft $5, hard $20, daily reset; configurable). Soft cap = one toast per period. Hard cap = next call refused (current LLM call finishes — no mid-stream cancellation; `approvals.md` forbids it as token-wasting).
- Air-gap: `[providers] outbound_allowed = false` rejects non-`Local` providers and contributes to `Boosted` posture.

### Approvals — Hard Invariant Reaffirmed

**No per-tool-call approval prompts. The agent just executes.** This survives the redesign unchanged.

- Tier system above is **audit categorization only** in v1 — never blocks, never prompts.
- T3 actions surface retroactively in `/status` and emit high-priority audit events. They do not interrupt the loop.
- Catastrophic denylist (`rm -rf /`, `mkfs`, `dd if=/dev/zero`, `curl|sh`) enforced at sandbox layer, no override.
- Budgets are the only blocker, and only at the boundary between LLM calls.
- T3 user prompting is deferred to **v3 (Sentry)**, gated on a future cargo feature.

### Posture v2 (Task 05)

Three states, multiplier applied to per-event XP (per-event clamp at 3 stays):

| State | Multiplier | Trigger |
|-------|------------|---------|
| `Reduced` | ×0.85 | Any deliberate downgrade: sandbox off, privileged channel, `blocked_paths` unset, denylist tampered, public listener with no auth. |
| `Default` | ×1.00 | Fresh install. Loopback only, sandbox on, audit chain on, no privileged channels, denylist intact. |
| `Boosted` | ×1.10 | All `Default` conditions plus ≥1 mTLS or Tailscale listener (daemon reachable from a paired remote device or tailnet). |

- Posture transitions emit `security_posture_changed` events into the HMAC hash chain. Rate-limited 2/hr (anti-flip-flop XP farming).
- Replay reads posture from the chain only, never live config — deterministic XP recomputation.
- `xp_gain` events carry `xp_breakdown` (base, align bonus, state multiplier, pre/post-clamp, final) and `posture_event_seq` cross-reference.
- UX: `/evolution` shows posture line; `/xp` header shows current posture + weekly avg multiplier; toast on transition. Discovery is opt-in.
- v1 status: types/resolver/snapshot/emit + daemon-startup reconcile + `Status::GetPosture` gRPC all shipped at `crates/cortex/src/evolution/security/` and `crates/borgd/src/daemon.rs`. Live config → `PostureSnapshot` → `posture_snapshots` index + `audit_log_chain` event with 2/hr rate limit. `Status::GetPosture` reads the head of `posture_snapshots` (falls back to `Default × 1.00` on a fresh DB). Replay multiplier wiring into `xp_gain` and the `/evolution` + `/xp` UX surfaces are deferred follow-ups (the resolver and `XpBreakdown` types are ready to receive them).

### Data Handling

- Retention config in `[data]`: `retention_days` (default `None` = forever), `purge_policy` (`auto` / `on_demand_only` / `never`), `backup_keep_count` (default 3, range 0–52). `DataConfig::should_auto_purge` encapsulates the interaction.
- Daily maintenance task (V37 cron `0 0 2 * * *`) calls `Database::purge_messages_before(cutoff)` (hard-delete) and `Database::tombstone_audit_events_before(cutoff, now)` (in-place tombstone) when `should_auto_purge()` is true. Counts surface on `MaintenanceReport.messages_purged` / `audit_events_tombstoned`.
- **Tombstone-on-purge in audit chain (V47):** the purge rewrites `event_json` to `{"purged":1,"purged_at":<ts>,"original_type":"<t>"}` and sets the new `event_purged` column. `prev_hash` and `hash` stay untouched as a forensic anchor. `verify_audit_chain` returns `OkWithTombstones { count }` for a chain with at least one tombstone, distinct from `Ok` (no purge fired) and `Tampered { seq }`. The verifier also catches "set the bit / leave the bit" mask attacks (a tombstoned row must carry the canonical payload, and vice versa). `borg audit verify` prints `OK`, `OK (N tombstones)`, or `TAMPERED at seq=N`.
- **Documented limitation:** an attacker with raw SQLite write access can edit `purged_at` inside a canonical tombstone payload without detection — by design (`hash` is the original anchor, not recomputed). Operators who need cryptographic detection of tombstone-payload edits compare against a backup taken before the purge.
- Backup unit (v1): single `borg-backup-<unix>.tar.zst` of `$BORG_HOME` (SQLite, blobs, skills, channels, hooks, config). **Excluded:** `borgd.sock`, `*.lock`, `logs/`, `backups/`. **mTLS private keys live in OS keychain and are NOT in the tarball** — restoring on a new host requires re-pairing.
- Tarball layout: outer `tar.zst` containing `metadata.json` (schema_version, created_at, sha256_pre_compress) + inner `body.tar` (the `borg_home/...` tree). `verify_backup` recomputes the sha256 over the inner tar before extracting and runs `verify_audit_chain` against the embedded SQLite. `restore_backup` refuses non-empty `BORG_HOME` without `--force`; `--force` renames existing dir to `.bak.<unix_ts>` and appends a `state_restored` audit event to the restored chain. Tar extraction is hardened (`set_overwrite(false)`, `set_preserve_permissions(false)`, `set_unpack_xattrs(false)`).
- Commands: `borg backup create`, `borg backup verify <file>`, `borg backup restore <file> [--force]`. Passphrase-encrypted backups are deferred (per `data-handling.md` open question #1).
- Self-healing: weekly auto-backup runs from the daily maintenance sweep when `backup_keep_count > 0` AND no backup exists in the last 7 days (`weekly_backup_due`). Older `borg-backup-*.tar.zst` files are pruned to `backup_keep_count` after a successful create. `backup_keep_count = 0` disables auto-backup and removes any stragglers. `MaintenanceReport.auto_backup_path` and `.backups_pruned` surface the result of each sweep.

### New Tables (v1)

- `listeners(id, bind, auth_backend, max_tier, fail_closed, enabled, created_at, updated_at)`
- `paired_devices(id, fingerprint, owner_label, key_storage_tier, scope_json, paired_at, last_seen_at, status, principal_id)`
- `revoked_fingerprints(fingerprint, revoked_at, reason)`
- `pairing_codes(code_hash, expires_at, intent, created_by)`
- `audit_log_chain(seq, prev_hash, hash, event_type, event_json, ts, principal_id, event_purged)` — hash-chained, blake3. `event_purged` (V47) marks tombstoned rows; chain hash includes the bit so flipping it without a matching payload edit is detected as `Tampered`.
- `posture_snapshots(id, evolution_event_id, taken_at, posture, source, schema_version)` — V47 denormalized index over `security_posture_changed` rows in `audit_log_chain`.
- `internal_ca_certs(serial, kind, pem, issued_at, expires_at, revoked_at)`
- `provider_rate_cards(provider_id, model, effective_at, …)` — versioned, replay-safe

### Task Status

| Task | Scope | Status |
|------|-------|--------|
| **01** Daemon kernel + gRPC + TUI client migration | `borgd` binary, `protocols`, `DaemonClient`, port TUI off in-process agent | **Complete** (capability router wiring tracked as a follow-up FIXME in `borgd/src/session/executor.rs`) |
| **02** mTLS + PAKE pairing + audit chain | `identity`, pairing flow, verifiers, hash-chained audit | Not started (depends on 01) |
| **03** Layer 5 adapters | Port 8 channels into `borgd`; OpenAI-compat HTTP; webhook; MCP server | Not started (depends on 01) |
| **04** Provider trait + rate cards + budgets | Unified `Provider`, versioned rate cards, soft/hard budgets, air-gap | **Partially landed** — V43/V44 schemas, `BudgetEngine` pre/post-flight, hourly reset tick, `borg providers add/list`, rate-card lookup with replay determinism, `/status` budget row, BYO endpoint registry, and `outbound_allowed` air-gap enforcement (now wired into `LlmClient::new` for primary + fallback slots) all shipped. Pre-flight estimate now uses agent-supplied `prompt_tokens` (tiktoken-based) via `HookData::LlmCall.prompt_tokens` instead of the old `message_count × 50` heuristic. **Deferred:** the per-provider `Provider` trait + 8 backend impls (still routing through monolithic `LlmClient`), `borg providers refresh-rates` signed-manifest verification (currently a `bail!` stub), and renaming `HookPoint::AfterLlmResponse` → `AfterLlmCall` to match the spec wording (post-flight is wired inline in `Agent::record_usage`). |
| **05** Posture v2 + data handling + cleanup | 3-state posture, retention, tombstone, backups, dead-code sweep, docs | **Mostly landed** — posture types/resolver/snapshot/emit, V47 schema, tombstone-on-purge with mask-attack guards, `DataConfig` + retention purge, `borg backup create/verify/restore` (hardened tar extraction), daemon-startup posture reconcile, `Status::GetPosture` reading the live head, and weekly auto-backup with cadence guard all shipped. Replay multiplier wiring into `xp_gain` events and the `/evolution` + `/xp` posture surfaces are the remaining deferred follow-ups. |

**Task 01 — landed so far** (commits `81476b4` → `ad28b67`):
- `borgd` daemon: flock single-instance, UDS listener, `Session`/`Capability`/`Status`/`Admin` services scaffolded.
- `SessionHost` per-session broadcast + ring buffer for resumable events.
- `PromptRegistry` for pairing/approval requests.
- `crates/protocols` with extended `Session`/`Admin` (multimodal Send with `Vec<InlineImage>`, agent control, scheduling, projects, pairing, sessions, popups).
- `DaemonClient` ergonomic wrappers (`send_message_with_images`, `get_state`, `compact`, `undo`, `rewind`, admin RPCs).
- `borg ask` REPL one-shot migrated to `DaemonClient` + pb→core event adapter.
- In-process gateway spawn from `service::` hooked into `borgd`.
- Per-session turn serialization; `close` cancels in-flight; concurrent `Send` no longer silently stalls.

**Task 01 — also landed (since the partial-status note above):**
- TUI runtime fully on `DaemonClient`; no `Arc<Mutex<Agent>>` in `crates/interface`.
- All TUI popups read/write via admin RPCs — no direct `Database::open()` from the TUI runtime. (CLI subcommands like `borg status` / `providers` / `webhook` still use `Database::open_readonly()` and are intentionally out of scope for Task 01.)
- `crates/gateway/src/handler.rs` runs through `SessionExecutor` (registry-backed `SessionHost`); no per-message `Agent::new`.
- Heartbeat scheduler + gateway server spawn live in `borgd` (`heartbeat.rs`, `orchestrator.rs`).
- `crates/interface/src/service/` and `crates/interface/src/in_flight.rs` deleted.
- Guard tests in place: `crates/interface/tests/grpc_client_smoke.rs` (no `rusqlite`/`cortex::db` from interface), `crates/borgd/tests/single_writer_lock.rs`, `crates/borgd/tests/capability_routing.rs`, plus `multi_client_broadcast.rs`.

**Task 01 — open follow-up:**
- `CapabilityRouter` is exercised by `capability_routing.rs` but its output isn't consulted by `AgentBackend` tool dispatch — `terminal.exec` / `clipboard.read` always run daemon-side. Behavior is safe (matches the spec's daemon-side fallback for `run_shell`), but client-side capability handling needs wiring before TUI shell aliases / clipboard work end-to-end. See FIXME in `crates/borgd/src/session/executor.rs`.

### Forward Compatibility (v2 mobile, v3 multi-tenancy/Sentry)

- **Every event carries `principal_id`** — defaulted to single device-owner in v1; v3 multi-tenancy is additive, not migrative.
- **Posture has `schema_version`** — v3 5-bucket ladder won't break v1 hash chains.
- **Capability protocol** — same protocol carries v2 mobile capabilities (camera, mic, biometric, location, push) and v3 enterprise verifiers (StaticX509, VaultPki, StepCa).
- **No design choice in v1 should preclude:** workspaces (per-user memory), agency-PKI verifiers, FIPS 140-3, SCIM/LDAP JML, M-of-N admin quorum, Shamir KEK escrow, T3 approval prompts, per-workspace budgets, NIST 800-53 crosswalk, `sentry-strict` cargo feature.
