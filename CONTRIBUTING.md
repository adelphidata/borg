# Contributing to Borg

Thanks for your interest in contributing! This guide will help you get set up and contributing quickly.

## Development Setup

1. Install [Rust 1.87+](https://rustup.rs/)
2. Clone the repository:
   ```sh
   git clone https://github.com/borganization/borg.git
   cd borg
   ```
3. Copy the environment template:
   ```sh
   cp .env.example .env
   ```
4. Add at least one LLM provider API key to `.env` — requires one of: `OPENROUTER_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `DEEPSEEK_API_KEY`, `GROQ_API_KEY`, or a running [Ollama](https://ollama.ai) instance (see [docs/configuration.md](docs/configuration.md) for details)
5. Build the project:
   ```sh
   cargo build --release
   ```
6. Run the onboarding wizard:
   ```sh
   ./target/release/borg init
   ```
7. Install git hooks (requires [lefthook](https://lefthook.dev/): `brew install lefthook` or `cargo install lefthook`):
   ```sh
   lefthook install
   ```

**Auto-rebuild on file changes** (install `cargo-watch` first with `cargo install cargo-watch --locked`):
```sh
cargo watch -x 'build --bin borg'
```

## Project Structure

Borg is a Cargo workspace with 7 crates:

| Crate | Purpose |
|-------|---------|
| `crates/cli` | Binary: REPL, clap args, TUI, onboarding |
| `crates/core` | Library: agent loop, LLM client, memory, config |
| `crates/heartbeat` | Library: proactive scheduler with quiet hours |
| `crates/sandbox` | Library: macOS Seatbelt + Linux Bubblewrap policies |
| `crates/apply-patch` | Library: patch DSL parser + filesystem applicator |
| `crates/gateway` | Library: webhook gateway for messaging channels |
| `crates/plugins` | Library: marketplace catalog, plugin installer |

See [docs/architecture.md](docs/architecture.md) for a deeper overview.

## Running Tests

```sh
cargo test                         # all tests
cargo test -p borg-apply-patch     # patch DSL tests
cargo test -p borg-core            # core tests
cargo test -p borg-gateway         # gateway tests
cargo test -p borg-plugins         # plugin tests
```

## Code Style

- Run `cargo fmt` before committing
- Run `cargo clippy -- -D warnings` and fix any warnings
- The lefthook `pre-commit` hook runs both in parallel; `pre-push` runs the full test suite

## Making Changes

1. Fork the repo and create a feature branch from `main`
2. Make your changes with clear, focused commits
3. Ensure all checks pass:
   ```sh
   cargo fmt --check
   cargo clippy -- -D warnings
   cargo test
   ```
4. Open a PR with a description of what changed and why

## Commit Messages

Use conventional prefixes (lowercase, no scope parens):

- `fix:` — bug fix
- `feat:` — new feature
- `refactor:` — code restructuring without behavior change
- `test:` — adding or updating tests
- `docs:` — documentation changes
- `chore:` — build, CI, or tooling changes

Example: `feat: add Discord native integration`

## Reporting Issues

- **Bugs:** Use the [bug report template](https://github.com/borganization/borg/issues/new?template=bug_report.md)
- **Features:** Use the [feature request template](https://github.com/borganization/borg/issues/new?template=feature_request.md)
- **Security:** See [SECURITY.md](SECURITY.md) — do not open public issues for vulnerabilities

## Questions?

Open a [discussion](https://github.com/borganization/borg/discussions).
