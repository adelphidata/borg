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

iMessage is macOS-only via `#[cfg(target_os = "macos")]`. Smaller crates (apply-patch, sandbox, heartbeat, plugins) enforce `#![warn(missing_docs)]`.

## Build & Test

```sh
cargo build
cargo fmt --check
cargo clippy -- -D warnings
cargo test --workspace --lib --bins 2>&1 | tee /tmp/borg-test.log
```

**Dev cadence.** Per-edit: `cargo build` + targeted `cargo test -p <crate> <new_test_name>` only. Skip `fmt --check`, `clippy`, full `--workspace` until ship-time (`/ship` runs them). CI re-runs everything. Investigation that needs >3 reads/greps → Explore subagent. Batch independent tool calls in one message.

**Run tests once.** `tee` to a log, then grep/tail. Use `--lib --bins` for feedback; full `--workspace` only for ship checks. Coverage: `just coverage`. CI gates at 80% via `cargo llvm-cov --fail-under-lines 80`.

**Live-LLM lane (opt-in).** `web_fetch` / `web_search` / `browser` real-network tests in `crates/cortex/tests/live_network_integration.rs`, gated by `live-llm` feature + `BORG_ALLOW_LIVE_TESTS=1`. **Live-MCP lane (opt-in).** Real stdio MCP servers (today: `uvx mcp-server-time`) in `crates/mcp/tests/live_mcp.rs`, gated by `live-mcp` feature + `BORG_ALLOW_LIVE_TESTS=1`. Default `cargo test` stays hermetic.

LLM credentials: one of OpenRouter, OpenAI, Anthropic, Bedrock, Azure OpenAI, Vertex AI, Gemini, DeepSeek, Groq, xAI, Ollama, or Claude Code CLI.

## Critical Invariants

### Error Handling — No Silent Swallowing

**NEVER silently discard errors with `let _ = ...` or `.ok()` on operations that matter.**

- Propagate with `?` when caller returns `Result`.
- Log + continue in fire-and-forget contexts; always `tracing::warn!` on the error path.
- `let _ = ...` ONLY for truly best-effort ops (typing indicators, terminal cleanup in Drop, temp file removal).
- If we report success to the user, it MUST actually succeed.

### UX — No Approval Prompts

**The agent just executes.** No per-tool-call approval prompts or confirmation dialogs. Shell commands auto-execute; only catastrophic commands (rm -rf /, mkfs, dd, curl|sh) are denied at the sandbox layer. Sandboxing and rate limiting are security boundaries, not approval dialogs.

### Mouse Interaction — Hard Invariant

**Native click+drag text selection MUST work in the transcript with no modifier keys.** Has regressed multiple times.

**Strategy:** inline viewport via `custom_terminal::Terminal::with_options` + `tui_lifecycle::Tui::insert_history_lines`. Trackpad/wheel scrolls the terminal pane natively. xterm Alternate Scroll Mode (`?1007h`) enabled on alt-screen surfaces. Ctrl+T opens transcript pager for full-text search.

**FORBIDDEN (will regress text selection):** `?1000h`, `?1002h`, `?1003h`, `?1006h`, crossterm `EnableMouseCapture`/`DisableMouseCapture`/`Event::Mouse`/`App::handle_mouse`/`MouseEventKind`, scrollbar click/drag. Guard tests in `crates/interface/src/tui/mod.rs` and `app.rs` use `include_str!` to detect reintroduced patterns — do not weaken.

### Tool Conservation

Every tool's JSON schema is sent every turn (~5KB+). Prefer adding actions/parameters to existing tools. If achievable via `run_shell` or an existing tool, don't create a new one.

### Test Quality — No Fluff Tests

**Tests must encode expected/spec behavior, not rubber-stamp current implementation.** Start from the spec/invariant; if the implementation is wrong, the test should fail.

**Every test must be able to fail for a real-world reason.** Reject:
- **Tautological** — asserting on values the test set, on `Default`/`Clone` output, or compile-time constants.
- **Over-mocked** — mock returns canned data; test asserts on canned data.
- **Smoke-only** — `.is_ok()` / `.is_some()` / "didn't panic" with no meaningful assertion.
- **Enum-variant-exists** — `assert!(matches!(x, Event::Foo { .. }))` is compiler-checked.
- **Source-grep tests** — only acceptable for invariants types can't express (prompt-cache and TUI mouse guards are exceptions, not templates).
- **Near-duplicate per-variant tests** — collapse into table-driven.

Every behavior-changing commit/PR includes tests. Bug fixes need a regression test.

### Patch DSL

`+` added, ` ` (space) context, `-` removed. Omitting the prefix is a common mistake.

```
*** Begin Patch
*** Add File: path/to/new.py
+import sys
*** Update File: path/to/file.py
@@
 context
-old
+new
*** Delete File: path/to/old.py
*** End Patch
```

Anchor ambiguous hunks with `@@ method_name` (or `@@ class Foo`). Stack anchors for nested scopes.

### mTLS — TCP Listener

`borgd --listen <addr>` requires mutual TLS. UDS is plaintext (filesystem perms 0600 own isolation).

- `build_server_tls_config` MUST be applied to any `tonic::Server` bound to a network socket. `auth_interceptor` MUST come from `auth::build_verifier` — never construct `TofuVerifier` inline.
- Clients connect via `daemon_client::connect_tcp`, loading three keychain PEMs (`CLIENT_CERT_KEY_ACCOUNT`, `CLIENT_KEY_KEY_ACCOUNT`, `CA_CERT_ACCOUNT`). Missing PEMs surface "run `borg device pair` first".

Server cert SAN is `localhost` — exposing beyond loopback needs operator's hostname added first. Guard tests in `crates/borgd/tests/mtls_handshake_e2e.rs`.

### Security — Blocked Paths

`[security] blocked_paths` defaults: `.ssh`, `.aws`, `.gnupg`, `.config/gh`, `.env`, `.envrc`, `credentials`, `private_key`, `id_rsa`, `id_ed25519`, `id_ecdsa`, `id_dsa`. Filtered from `fs_read`/`fs_write` and checked in `list_dir`. Matching is per path-component with **dotted-family** coverage on the final component: a `.env`/`private_key`/`credentials` entry also blocks `.env.local`, `private_key.pem`, `credentials.json` (component-suffix `entry.` — not substring, so `.environment`/`.sshrc` are not over-blocked).

### KEK Backend — Headless VPS Default

`security.kek_backend` (`auto` | `os_keychain` | `file`, default `auto`). On `Auto`, probes OS keychain; falls back to `FileKekStore` (passphrase-encrypted at `~/.borg/secrets/kek.enc`, passphrase from `BORG_KEK_PASSPHRASE`). File backend posts as `Reduced` (×0.85 XP); `borg service install --secure` upgrades on Debian/Ubuntu. Agent always boots.

### Subsystem Supervision

**Bare `tokio::spawn` for new long-running daemon subsystems is forbidden** — use `spawn_supervised` from `crates/borgd/src/supervisor.rs`. Panic-catching, exponential backoff respawn, degraded-state cap (5 restarts in 10min → Fail). Heartbeat scheduler loop is panic-isolated per-iteration with a 60s liveness counter.

### Hooks Can Never Break the Agent

`PreToolUse` non-zero exit aborts the tool call. All other hook failures log + continue. User scripts at `~/.borg/hooks.json` (Claude Code-compatible).

## Systems Reference (file pointers — read the code for detail)

| System | Lives in | Notable gotcha |
|---|---|---|
| Tools | `crates/cortex/src/tool_definitions.rs` + `tool_handlers/` | `schedule.create` `pre_run_command` injects stdout as `<preprocessed_context>` before LLM; `[SILENT]` first line suppresses channel delivery |
| Goals | `crates/cortex/src/goals/` | Per-session Ralph loop; `goals` row is source of truth; sub-agents do NOT inherit parent goal |
| Multi-Agent | `crates/cortex/src/multi_agent/` | `toolsets: []` ≠ None (means "no tools"); orchestrator role requires `agents.orchestrator_enabled` + depth check; Plan-mode children stay read-only even at orchestrator role |
| Config | `crates/cortex/src/config/` | DB-only, no config.toml; add fields to `<section>.rs` + `define_settings!` in `settings_table.rs` |
| Memory | `crates/cortex/src/memory/` | Two-tier (session + `memory_entries`); `write_memory` enforces 20k-token cap + prompt-injection scan; `memory.provider` (default `native`) selects backend; external impls fall back to native with one-shot warn |
| Skills | `crates/cortex/skills/` + `crates/cortex/src/skills.rs` | SKILL.md + YAML frontmatter; user skills at `~/.borg/skills/<name>/` override built-ins |
| Channels | `crates/gateway/src/<name>/` | Per-channel/sender LLM routing via `[[gateway.bindings]]`; thread-scoped session key `{sender_id}:{thread_id}` |
| Collaboration Modes | `crates/cortex/templates/collaboration_mode/` | Execute (default) + Plan (read-only); "Default" mode removed — `"default"` is a `FromStr` legacy alias only |
| Agent Loop | `crates/cortex/src/agent/` | `<internal>` tags stripped real-time; loop safety: `ToolCallRepeatTracker` (Allow→Backoff→Block, cap `loop_guard.identical_call_max=3`), `UnknownToolTracker` (soft error then terminal at `unknown_tool_max_per_name=2`) |
| Heartbeat | `crates/heartbeat/` + `crates/borgd/src/heartbeat.rs` | Two schedulers: `heartbeat::scheduler` is tick generator only, `borgd::scheduler` is the dispatcher — don't merge. `heartbeat.min_interval_secs=60` in production. Skips cleanly when `llm.provider` unset (`SkipReason::NoLlmProvider`) |
| Database | `crates/cortex/src/db/` | libsql (NOT rusqlite/turso) at `~/.borg/borg.db`. **Cleartext at rest today** — every production caller uses `Database::open()`; the SQLCipher AES-256-CBC lane (`open_encrypted`, key HKDF-derived from KEK) is wired but **dormant** (enabling it needs a migration — plaintext DBs can't be read back through the key). DB confidentiality currently rests on `~/.borg` 0700 perms + sandbox deny-read, not encryption; KEK/CA/device keys are encrypted separately under `secrets/`. Don't describe the DB as encrypted at rest until `open_encrypted` is on the boot path |
| Sandboxing | `crates/sandbox/` | macOS `sandbox-exec` Seatbelt (deny-all default); Linux `bwrap` namespace |
| MCP Client | `crates/mcp/` + `crates/cortex/src/mcp_clients.rs` | Borg consumes external MCP, does NOT expose itself. Stdio children get only `ENV_ALLOWLIST` — every API_KEY/passphrase/OAuth stripped before spawn; credentials via `${SECRET:NAME}` only. HTTPS-only by default. Cloud-metadata FQDNs (`metadata.google.internal`, `metadata.goog`) stay refused even when `mcp.allow_local_network=true` — flipping that knob must not unlock SSRF to IMDS. Streamable-HTTP captures + echoes `Mcp-Session-Id`, drops on 404 and tears down the session (re-init = daemon restart today). Server-initiated requests (sampling/createMessage, roots, …) refused with JSON-RPC `-32601`. Per-call output capped at `mcp.max_response_bytes` (default 1 MiB; 0 → falls back to default, not unbounded). Plan-mode default-blocks MCP tools; only `tools.read_only` allowlist admitted (server `readOnlyHint` is NOT trusted for this). Mutating verbs CLI-only so trust gauntlet runs in one path. Audit events: `mcp_server_started/failed`, `mcp_tool_registered`, `mcp_schema_rejected`, `mcp_tool_executed`, `mcp_rate_limited`, `mcp_response_truncated`, `mcp_sampling_refused`. OTEL events under `borg.mcp` target |
| WASM Plugins | `crates/plugins/src/wasm.rs` | Tool-only (channels/hooks/onboarding stay native). Install gauntlet: manifest → SHA-256 pin → Ed25519 sig → static walk → HMAC seal. Runtime `wasmtime` with fuel/memory/output/wall-clock caps. No WASI/sockets/subprocesses/FS. Auto-reseals legacy-framing installs |
| Vitals & Posture | `crates/cortex/src/vitals/` | 5 stats from HMAC-verified events; Posture v2: Reduced ×0.85, Default ×1.00, Boosted ×1.10; replay reads chain only |
| Lifecycle Hooks | `crates/cortex/src/hooks.rs` | 9 `HookPoint` variants; compiled: Vitals/Activity/Bond/Evolution/Budget |
| LLM Resilience | `crates/cortex/src/llm/` | Credential pool (per-key cooldown via `Retry-After`, 401/403 permanent removal, 402 disambiguation) + `llm.fallback` chain + `llm.aux` background client. OAuth sources first-class |
| Self-Healing | `crates/cortex/src/maintenance/` | Daily 02:00 — prune logs, evict embeddings, heal stalled tasks. No LLM call. Sweep also runs `PRAGMA wal_checkpoint(TRUNCATE)` always + conditional `VACUUM` (fragmentation >15% AND `maintenance.vacuum_interval_days=7` elapsed). Mutating actions capped by `maintenance.heal_action_budget=1000` |
| Prompt Injection | various | 5 layers: input sanitization, XML trust boundaries, prompt hardening, rate limiting, secret redaction |
| Git Utilities | `crates/cortex/src/git.rs` | Ghost commits (snapshot via temp index, atomic undo) |

## Debugging

Logs at `~/.borg/logs/`: `tui.log`, `daemon.log`/`daemon.err`, `{date}.jsonl`. Settings in `~/.borg/borg.db` settings table (inspect via `borg settings get/set/unset` — no config.toml). `RUST_LOG=debug` for module-level control.

## Adding New Things

### New Setting

1. Add Rust field + default to `crates/cortex/src/config/<section>.rs`.
2. Add line to `define_settings!` in `crates/cortex/src/config/settings_table.rs` (macro generates registry + `apply_setting()` arm + TUI popup row).

For `/settings` popup: add to `tui_settings { … }` block: `"my.setting" => "Label", Bool, "Category";`. Kinds: `Bool`, `Float`, `Uint`, `Select`.

### New Tool

Strongly prefer extending an existing tool. If you must add:
- Definition: `crates/cortex/src/tool_definitions.rs`
- Handler: `crates/cortex/src/tool_handlers/<name>.rs`
- Group: `crates/cortex/src/tool_catalog.rs` (`tool_group()`)
- Plan mode: defaults blocked — add to allowlist in `agent.rs` if non-mutating

### New Skill

`crates/cortex/skills/<name>/SKILL.md` with YAML frontmatter (name, description, requires.bins, requires.env). Register `include_str!` in `crates/cortex/src/skills.rs`.

### New Native Channel

`crates/gateway/src/<name>/` with types/parse/api/verify modules. Register in `crates/gateway/src/server.rs` and `crates/gateway/src/channel/channel_init.rs`.
