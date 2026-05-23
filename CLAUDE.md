# Borg

AI personal assistant agent in Rust, security-first by default. The agent writes its own tools at runtime. Single binary, all integrations compiled in. Data dir: `~/.borg/`.

## Architecture

Cargo workspace, no-prefix crate names:

| Crate | Purpose |
|-------|---------|
| `interface` | Binary `borg`: REPL, TUI, clap, onboarding |
| `cortex` | Agent loop, LLM, memory, identity, config, tools, skills |
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

**Run tests once.** `tee` to a log, then grep/tail. Use `--lib --bins` for feedback; full `--workspace` only for ship checks. Coverage: `just coverage` / `just coverage-summary`. CI gates at 80% via `cargo llvm-cov --fail-under-lines 80` in `.github/workflows/ci.yml`.

**Live-LLM lane (opt-in).** `web_fetch` / `web_search` / `browser` are exercised against real network targets in `crates/cortex/tests/live_network_integration.rs`, gated by both the `live-llm` cargo feature and `BORG_ALLOW_LIVE_TESTS=1`. Default `cargo test` stays hermetic; full ship checks should run `BORG_ALLOW_LIVE_TESTS=1 cargo test -p cortex --features live-llm --test live_network_integration`.

Requires one of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, `XAI_API_KEY`, `AZURE_OPENAI_API_KEY` or Azure Entra ID (with `llm.azure_resource` / `llm.azure_deployment` / `llm.azure_api_version` set), AWS credential chain (Bedrock), Google Vertex AI service-account / metadata credentials (with `llm.vertex_project` set), Ollama, or Claude Code CLI.

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

**Strategy:** inline viewport via `custom_terminal::Terminal::with_options` plus `tui_lifecycle::Tui::insert_history_lines`. The terminal's normal scrollback owns prior history; trackpad/wheel scrolls the terminal pane natively. xterm Alternate Scroll Mode (`?1007h`) is enabled on entry to alt-screen surfaces (transcript pager, large popups) so wheel events translate to arrow keys there.

**Behavior:** Click+drag selects text natively. Trackpad/wheel scrolls the terminal's scrollback. Up/Down always navigate composer history (no scroll-offset disambiguation). Ctrl+T opens transcript pager in alt-screen for full-text search. PageUp/PageDown/Home/End are no-ops at the app level — terminal handles them.

**FORBIDDEN (will regress text selection):**
- Escape sequences: `?1000h`, `?1002h`, `?1003h`, `?1006h`
- Crossterm: `EnableMouseCapture`, `DisableMouseCapture`, `Event::Mouse`, `App::handle_mouse`, `MouseEventKind`
- Scrollbar click/drag (requires `?1000h`)

Guard tests in `crates/interface/src/tui/mod.rs` and `app.rs` use `include_str!` to detect reintroduced patterns. Do not weaken.

### Tool Conservation

Every tool's JSON schema is sent to the LLM every turn (~5KB+). Prefer adding actions/parameters to existing tools. If achievable via `run_shell` or an existing tool, don't create a new one.

### Test Quality — No Fluff Tests

**Tests must encode expected/spec behavior, not rubber-stamp current implementation.** A test that's written by reading the code and asserting whatever it currently does will pass for the wrong reasons and silently lock in bugs. Start from the spec/invariant you intend to hold, then assert against it — if the implementation is wrong, the test should fail.

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

When the same context lines could match multiple places, anchor the hunk with `@@ method_name` (or `@@ class Foo`) before `@@`. Stack multiple anchors to navigate nested scopes:

```
*** Update File: src/parser.rs
@@ impl Parser
@@ fn parse
@@
     ...
-    old_line
+    new_line
```

### mTLS — TCP Listener

The daemon's TCP listener (`borgd --listen <addr>`) requires mutual TLS: every connecting client MUST present a CA-signed cert. UDS is plaintext — filesystem permissions (0600 socket) own the isolation there. Two non-negotiables when touching the transport layer:

- `build_server_tls_config` MUST be applied to any `tonic::Server` bound to a network socket. The `auth_interceptor` MUST come from `auth::build_verifier` — never construct `TofuVerifier` inline (silently bypasses `security.verifier`).
- Clients connect via `daemon_client::connect_tcp`, which loads the three keychain-stashed PEMs (`CLIENT_CERT_KEY_ACCOUNT`, `CLIENT_KEY_KEY_ACCOUNT`, `CA_CERT_ACCOUNT`) and presents them to the server. Missing PEMs surface a "run `borg device pair` first" error rather than a generic TLS failure.

Guard tests live in `crates/borgd/tests/mtls_handshake_e2e.rs`: no-cert + foreign-CA-cert clients are rejected; paired clients succeed; revoked fingerprints fail at the interceptor with a `grpc_auth_failure` audit row.

**Loopback-only today.** The server cert SAN is `localhost` (`ensure_server_cert` at `daemon.rs`). Connecting to `--listen 0.0.0.0:PORT` from a non-loopback peer will fail SNI verification — exposing borgd beyond loopback needs the operator's hostname added to the SAN before it's usable.

### Security — Blocked Paths

`[security] blocked_paths` defaults: `.ssh`, `.aws`, `.gnupg`, `.config/gh`, `.env`, `credentials`, `private_key`. Filtered from `fs_read`/`fs_write` and checked in `list_dir`.

### KEK Backend — Headless VPS Default

`security.kek_backend` (`auto` | `os_keychain` | `file`, default `auto`). On `Auto`, `borgd` probes the OS keychain at first start; if unavailable (headless Linux without dbus + secret-service) it falls back to `FileKekStore` (passphrase-encrypted file at `~/.borg/secrets/kek.enc`, passphrase from `BORG_KEK_PASSPHRASE` env / systemd EnvironmentFile). The file backend posts as `Reduced` (×0.85 XP); `borg service install --secure` upgrades to gnome-keyring on Debian/Ubuntu and fires the one-time `secure_headless_setup` milestone (+100 XP). On non-Debian, `--secure` falls back to the file backend without erroring — the agent always boots.

## Systems Reference

### Tools

`write_memory`, `read_memory`, `memory_search`, `list`, `apply_patch`, `run_shell`, `read_file`, `list_dir`, `browser`, `web_fetch`, `web_search`, `schedule`, `request_user_input`, `text_to_speech`, `generate_image`. Definitions in `crates/cortex/src/tool_definitions.rs`. Dynamic tools from MCP servers (`mcp_<server>_<tool>`) and WASM plugins are merged at the same call site; see `docs/mcp.md` and the **MCP Client** section below.

`schedule.create` for `type=prompt` accepts an optional `pre_run_command` (with `pre_run_cwd`, `pre_run_timeout_ms` defaulting to 60s). Stdout is injected into the prompt as a `<preprocessed_context source="pre_run" exit_code="N">` block before the LLM runs; non-zero exit / timeout / spawn failure surface as `status="failed"` and the LLM still runs — silent preamble failures lose user trust. Stdout/stderr capped at 64KB. If the LLM response's first non-empty line equals `[SILENT]`, channel delivery is suppressed.

### Goals

Per-session Ralph-loop objective. The DB row in `goals` is the source of truth; `GoalRuntimeState` (`crates/cortex/src/goals/runtime.rs`) caches the active `goal_id`, per-turn token/time baseline, and a one-shot steering queue. Each iteration the agent loop reads the row, calls `adopt()` to detect mid-flight objective swaps, marks the turn baseline (skipped in Plan mode), drains pending steering into history before the LLM call, charges `db.account_goal_usage` at `EndTurn` (atomic Active→BudgetLimited flip), and self-continues by injecting `continuation_prompt` whenever `should_continue(status, pending, plan_mode)` is true. Continuation is capped to one per turn-id via `try_reserve_continuation`.

Model-visible tools: `get_goal` (read-only, Plan-mode OK), `create_goal`, `update_goal` (mark complete only). Operator surface: `/goal [pause|resume|clear|<text>]` via Admin gRPC `GetGoal` / `SetGoal` / `ClearGoal` / `PauseGoalOnInterrupt`. Lifecycle telemetry (`goal_created`, `goal_completed`, `goal_budget_limited`) is recorded as `EventCategory::Telemetry` vitals events with zero stat deltas — pure metrics, no posture multiplier change. The TUI footer subscribes to `SubscribeGoalEvents` for sub-100ms paused/budget-limited indicators. Sub-agents (`spawn_agent`) get a fresh `session_id` and intentionally do NOT inherit the parent goal.

### Config

DB-only — `crates/cortex/src/config/`. All settings in the `settings` table. Two-tier: DB → compiled defaults. `Config::load_from_db()` builds runtime; `SettingsResolver` for set/get/unset. No config.toml.

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

**Execute** (autonomous, the default) and **Plan** (read-only, blocks mutating tools). `/plan` toggles with auto-restore. Templates in `crates/cortex/templates/collaboration_mode/`. The legacy "Default" mode was removed — Borg is autonomous; `"default"` survives only as a `FromStr` alias for legacy DB rows. See the guard test `collaboration_mode_default_variant_must_not_return`.

### Agent Loop

`core/agent.rs` — streams LLM, parses tool calls, executes, loops until text-only. `<internal>` tags stripped in real-time. Messages persisted to the DB immediately. System prompt assembled per turn from IDENTITY + time + git + mode + memory + project docs + skills.

**Loop safety guards.** Two per-turn trackers in `agent/tool_validation.rs` reset at every `run_agent_loop` start. `ToolCallRepeatTracker` hashes `(name, args)` and escalates identical streaks through `Allow → Backoff(jittered exponential, ±25%) → Block`; the hard cap (`loop_guard.identical_call_max`, default 3) stays the terminal safety net while backoff buys an operator-interrupt window. `UnknownToolTracker` counts per-name hallucinated-tool attempts; the first attempt surfaces the soft "Unknown tool: X" error (typo recovery), `loop_guard.unknown_tool_max_per_name` (default 2) flips to a terminal "does not exist" refusal so the model stops retrying. Together they bound the runaway-model failure mode that would otherwise burn the whole 25-iteration cap.

### Heartbeat

Proactive check-ins (default 60m or cron, quiet 00:00–06:00). `~/.borg/HEARTBEAT.md` checklist. Honors gateway bindings. Suppresses empty/duplicate/ack-only. Trigger: `borg poke` / `/poke`. `heartbeat.min_interval_secs` (default 60, hard floor 1) is the operator/eval-suite knob for sub-minute firings — leave at 60 in production.

**Two schedulers, two roles.** `heartbeat::scheduler` is a pure tick generator: cron + quiet-hour evaluation, emits fire events, no task execution. `borgd::scheduler` is the dispatcher: consumes ticks plus DB-stored `scheduled_tasks`, and hands work to `borgd::task_runner`. Don't merge them — the heartbeat crate stays embeddable/testable without the daemon, and `borgd` owns the side-effecting execution path.

### Database

libsql (SQLite-C fork, `libsql` crate — not `rusqlite`/`turso`) at `~/.borg/borg.db`. Migrations run on `Database::open()`. Tables: sessions, messages, scheduled_tasks, channel_*, settings, pairing_requests, approved_senders, embedding_cache, vitals_events.

Multi-process WAL is real: borgd holds the writer; standalone `borg` CLI processes can open the same DB concurrently without hitting a locking error. Encryption-at-rest is SQLCipher (AES-256-CBC) via `Database::open_encrypted`; the DB key is HKDF-derived from the KEK and never persisted. Existing DBs that were encrypted under the prior turso backend (AES-256-GCM, hex-string key) are not readable post-migration — the codebase is treated as fresh-install acceptable.

### Sandboxing

macOS: `sandbox-exec` Seatbelt (deny-all default). Linux: `bwrap` namespace isolation. Policy from `[sandbox]` per script/channel. Blocked paths filtered before profile generation.

### MCP Client

Borg consumes external MCP servers — it does **not** expose itself as an MCP server from the agent runtime. Four transports work: **stdio**, **streamable-http**, **http+sse** (legacy), and **HTTP/2** (negotiated via TLS ALPN on either HTTP transport). The protocol layer lives in `crates/mcp/`; the cortex adapter (`crates/cortex/src/mcp_clients.rs`) owns init, trust wrapping, rate limiting, and dispatch.

**Trust model.** MCP servers are *untrusted code at arm's length*. Stdio children get only the env allowlist in `crates/mcp/src/transport/mod.rs::ENV_ALLOWLIST` — every `*_API_KEY`, `BORG_KEK_PASSPHRASE`, and OAuth token is stripped before spawn; credentials reach a server only via `${SECRET:NAME}` placeholders the operator explicitly declares. HTTP URLs are `https://`-only by default and refuse loopback/RFC1918 unless `mcp.allow_local_network` flips. Every tool result is wrapped `<mcp_tool_result trust="untrusted">…</mcp_tool_result>` after passing the existing secret-redaction pipeline. Per-server token-bucket rate limiting fails closed.

**Plan mode trust gate.** MCP tools default to blocked in Plan mode; only tools the operator has explicitly named in per-server `tools.read_only` config are admitted. Server-advertised `annotations.readOnlyHint` is **not** trusted for this — it informs Execute-mode parallel-fanout batching only.

**Operator surface.** `borg mcp {list,add,add-preset,remove,enable,disable,test}`. The `test` verb is standalone — it connects, runs `tools/list`, prints diagnostics, and exits without touching `borgd`. Built-in presets: `time`, `filesystem`, `github`, `notion` (Hermes parity). Kill switches: `mcp.enabled`, `mcp.disabled_servers`, `mcp.max_tools_total`. See `docs/mcp.md`.

### WASM Plugin Sandbox

Third-party tool plugins ship as signed, capability-sandboxed WebAssembly modules. **Tool-only by design** — channels, hooks, onboarding, and background tasks are first-class native subsystems and intentionally do not run as WASM plugins. The sandbox exists to accept *untrusted publisher-signed code*; those other surfaces carry operator credentials and lifecycle responsibilities that don't fit a marketplace trust model. Code in `crates/plugins/src/wasm.rs`. Schema tag `borg.plugin.v1`.

Install gauntlet: manifest validation → WASM SHA-256 pin → Ed25519 publisher signature (operator-trusted publisher list) → static WASM walk (rejects WASI, undeclared host imports, `start` functions, `memory64`/shared memory, oversized memories, missing ABI exports `memory`/`alloc`/`borg_tool_call`, non-HTTPS endpoints, unknown capabilities) → local HMAC seal over canonical manifest + WASM. Every execution re-checks the seal; tampering quarantines the plugin instead of running it. The runtime also auto-reseals installs that were sealed under the pre-current ("legacy") HMAC framing so operators are not forced to reinstall on upgrade (`wasm_plugin_resealed` audit event).

Runtime is `wasmtime` with fuel + memory pages + output bytes + wall-clock caps from the manifest (host ceilings: `fuel ≤ 50M`, `max_memory_pages ≤ 256`, `max_output_bytes ≤ 1 MiB`, `timeout_ms ≤ 60_000`). No WASI, sockets, subprocesses, or filesystem. Host imports are gated by capability: `borg:log` (log), `borg:http` (http — endpoint allowlist + secret-leak scan at host boundary), `borg:state` (state), `borg:credentials` (credentials). Capability/endpoint/credential coherence is enforced both ways — declaring `http` requires at least one `https://` endpoint, and vice versa. See `docs/plugins.md`.

**Operator surface.** `plugins.wasm.enabled` is the global kill switch. `plugins.wasm.disabled_plugin_ids` and `plugins.wasm.disabled_publisher_keys` hide individual plugins / every plugin signed by a key — runtime tools are filtered and dispatch refuses. `plugins.wasm.rate_limit_per_minute` (default 60) + `plugins.wasm.rate_limit_burst` (default 10) cap per-plugin throughput; refused calls emit `wasm_plugin_rate_limited`. `security.deprecated_publisher_keys` schedules retirement: pre-sunset installs print a warning, post-sunset installs reject. CLI subcommands: `borg plugin disable/enable <id>`, `borg plugin disable-publisher/enable-publisher <key>`. OpenTelemetry metrics: `borg.wasm.executes_total{outcome}`, `borg.wasm.execute_duration_ms`, `borg.wasm.quarantines_total{reason_category}`, `borg.wasm.cache_hits_total`/`misses_total`, `borg.wasm.rate_limited_total`, `borg.wasm.resource_exhausted_total{kind}`, `borg.wasm.resealed_total`. Compiled-module cache uses LRU eviction (cap 64) so hot plugins survive churn.

### Vitals & Posture

5 stats (stability, focus, sync, growth, happiness) computed by replaying HMAC-verified events. `VitalsHook` on SessionStart/BeforeAgentStart/AfterToolCall. `borg status` / `/status`.

Posture v2: 3 states with XP multiplier — `Reduced` ×0.85, `Default` ×1.00, `Boosted` ×1.10. Transitions emit `security_posture_changed` into hash chain (rate-limited 2/hr). Replay reads from chain only, not live config.

### Lifecycle Hooks

One `Hook` trait / `HookRegistry` / 9 `HookPoint` variants in `crates/cortex/src/hooks.rs`. Compiled-in: `VitalsHook`, `ActivityHook`, `BondHook`, `EvolutionHook`. User scripts at `~/.borg/hooks.json` (Claude Code-compatible schema). `PreToolUse` non-zero exit aborts the tool call. All other failures log + continue — **hooks can never break the agent**. See `docs/hooks.md`.

### LLM Resilience

Three-layer recovery: credential pool (per-provider keys with cooldown TTL + strategy), `llm.fallback` provider chain, and the `llm.aux` background client. The pool replaces FIFO key rotation — per-key cooldown via `Retry-After` (header → body hint → configured default), permanent removal on 401/403, persistence across restart, 402 disambiguation (transient usage-limit vs hard exhaustion). OAuth sources (Claude Code, OpenAI OAuth, Copilot, Qwen-CLI) and custom OpenAI-compatible endpoints are first-class credentials in the pool; access tokens auto-refresh, and provider fallback defers while the pool can still recover from a transient rate-limit. Knobs: `llm.credential_strategy`, `llm.credential_cooldown_secs`, `llm.max_concurrent_per_credential`. State tables: `credential_pool`, `credential_suppressions`. Full surface in `docs/llm-resilience.md`.

### Self-Healing

Daily maintenance task fires 02:00 — prunes logs, evicts stale embeddings, heals stalled tasks, surfaces persistent warnings. No LLM call. Daemon scans every 5min for tasks with `next_run` drift > 1h. `write_memory` 20k-token cap enforced. `skill_audit` table catches user SKILL.md tampering. `/heal` invokes on demand. See `docs/self-healing.md`.

Each sweep also runs `PRAGMA wal_checkpoint(TRUNCATE)` (always) and conditional full `VACUUM` (when fragmentation > 15% and `maintenance.vacuum_interval_days` (default 7) has elapsed). Mutating actions per sweep are capped by `maintenance.heal_action_budget` (default 1000) — a backstop against runaway repair loops; remaining steps short-circuit on exhaustion and the budget surfaces as a persistent warning after two consecutive exhausts.

### Subsystem Supervision

Every long-running task in `borgd` runs under `crates/borgd/src/supervisor.rs`'s `spawn_supervised` wrapper: panic-catching, exponential backoff respawn, degraded-state cap (5 restarts in 10min → marked Fail in `maintenance_issue_state`). The heartbeat scheduler's loop is panic-isolated per-iteration via `catch_unwind` plus a 60s liveness counter that an external watchdog uses to detect stalls. **Bare `tokio::spawn` for new long-running daemon subsystems is forbidden** — use `spawn_supervised`. See `docs/daemon-supervision.md`.

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

`crates/gateway/src/<name>/` with types/parse/api/verify modules. Register in `crates/gateway/src/server.rs` and `crates/gateway/src/channel/channel_init.rs`.
