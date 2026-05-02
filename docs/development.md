# Development

Guide for building, testing, and working on the Borg codebase.

## Prerequisites

- Rust stable toolchain (install via [rustup](https://rustup.rs/))
- `cargo fmt` and `cargo clippy` (included with rustup)
- Linux: `bubblewrap` package for sandbox tests

## Building

```sh
cargo build            # debug build
cargo build --release  # optimized build
```

The binary is `target/release/borg` (or `target/debug/borg`).

## Testing

```sh
cargo test                            # all tests
cargo test -p coding        # patch parser + applicator
cargo test -p cortex               # config + skills + memory + settings tests
cargo test -p heartbeat          # heartbeat scheduler tests
cargo test -p sandbox            # sandbox policy + runner tests
cargo test -p gateway            # channel manifest + registry tests
cargo test -p plugins            # catalog + installer tests
```

## Linting

```sh
cargo fmt --check          # formatting check
cargo clippy -- -D warnings  # lint check (warnings are errors)
```

## Project structure

Cargo workspace under `crates/` (no-prefix names):

| Crate | Purpose |
|-------|---------|
| `interface` | The `borg` binary: clap commands, REPL, ratatui TUI, onboarding wizard |
| `cortex` | Agent loop, LLM client, memory, identity, config, tools, skills, workflows, hooks, evolution, bond — the bulk of the app |
| `borgd` | Long-running gRPC daemon: scheduler, task runner, channel delivery, heartbeat, orchestrator, mTLS pairing |
| `heartbeat` | Heartbeat scheduling primitives (interval/cron, quiet hours, poke signal) |
| `gateway` | Webhook gateway + native channel integrations (Telegram, Slack, Discord, Teams, Google Chat, Signal, Twilio, iMessage) |
| `sandbox` | macOS Seatbelt + Linux Bubblewrap policy + runner |
| `coding` | Patch DSL parser + filesystem applicator |
| `plugins` | Plugin marketplace catalog + installer |
| `protocols` | tonic gRPC schema |
| `identity` | Internal CA, cert lifecycle, OS keychain wrapping |

Source paths to know:

- `crates/cortex/src/agent/` — agent loop and turn execution
- `crates/cortex/src/tool_handlers/` — built-in tool implementations
- `crates/cortex/src/tool_definitions.rs`, `tool_catalog.rs` — tool schemas + groups
- `crates/cortex/src/config/` — settings registry + DB-backed resolver
- `crates/cortex/src/db/` — SQLite schema, migrations, per-table helpers
- `crates/cortex/src/memory/` — short/long-term memory + hybrid search
- `crates/cortex/src/skills.rs` + `crates/cortex/skills/` — skill loader + bundled skill bodies
- `crates/cortex/src/workflow/` — multi-step orchestration
- `crates/cortex/src/hooks.rs` — lifecycle hook trait + registry
- `crates/cortex/src/evolution/`, `crates/cortex/src/bond/`, `crates/cortex/src/vitals.rs` — XP / autonomy tier / vitals
- `crates/interface/src/tui/` — TUI rendering (mouse/keyboard invariants live here — see CLAUDE.md)
- `crates/borgd/src/{daemon,scheduler,task_runner,heartbeat,orchestrator,channel_delivery}.rs` — daemon-side runtime

## Runtime requirements

The binary requires one of `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, or a running Ollama instance at runtime. For development, create a `.env` file from the example:

```sh
cp .env.example .env
# edit .env with your key
```

## Cost events

`borgd` exposes an opt-in gRPC stream at `Telemetry.StreamCostEvents`.

- Gate: `telemetry.cost_events` in the DB-backed settings table, default `false`
- Transport/auth: unchanged from the rest of the daemon gRPC surface
- Event: one `CostEvent` per completed LLM call
- Source labels:
  - `openrouter_usage` when OpenRouter returns `usage.cost`
  - `table_openai` when OpenAI cost is derived from the static price table
  - `table_anthropic` when Anthropic cost is derived from the static price table
  - `unknown` when cost is unavailable or the model is not priced
- `cost_usd_present = false` means “unknown”, not `$0`

OpenRouter only returns `usage.cost` when the request opts in with
`usage.include = true`. Borg only adds that field when
`telemetry.cost_events = true`; when the setting is off, no cost-event bus is
created and the OpenRouter request shape stays on the existing path.

Emission is best-effort by design.

- Cost events never block the LLM path.
- Lagging subscribers can miss events; dropped samples increment the
  `cost_events_dropped` metric.
- Sink failures and panics are contained and logged; they do not fail the
  user-visible LLM call or terminate the daemon.

## Adding a built-in skill

1. Create `crates/cortex/skills/<name>/SKILL.md` with YAML frontmatter
2. Add `const BUILTIN_<NAME>: &str = include_str!("../skills/<name>/SKILL.md");` in `crates/cortex/src/skills.rs`
3. Add the constant to the `builtins_raw` array in `load_all_skills()`
4. Add a test case in the `builtins_parse_correctly` test

## Adding a new crate

1. Create the crate directory under `crates/`
2. Add it to the workspace `members` in the root `Cargo.toml`
3. Add workspace lint configuration
4. Wire up dependencies from existing crates as needed

## Database

SQLite at `~/.borg/borg.db`. Schema version is tracked in the `meta` table; migrations run automatically on `Database::open()`. See `crates/cortex/src/db/migrations.rs` for the canonical list of tables and the per-step DDL — that file is the source of truth, kept in sync with the code.
