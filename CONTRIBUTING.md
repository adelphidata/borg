# Contributing to Borg

Thanks for your interest in contributing! This guide will help you get set up and contributing quickly.

## Development Setup

1. Install [Rust 1.87+](https://rustup.rs/)
2. Clone the repository:
   ```sh
   git clone https://github.com/adelphidata/borg.git
   cd borg
   ```
3. Copy the environment template:
   ```sh
   cp .env.example .env
   ```
4. Add at least one LLM provider API key to `.env` — see `.env.example` for the full list of supported providers, or run a local [Ollama](https://ollama.ai) instance
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

**Run during development** (via [just](https://github.com/casey/just)):
```sh
just run            # TUI/CLI client (borg)
just daemon         # gRPC daemon (borgd)
just check          # fmt + clippy
just test           # all tests
```

**Auto-rebuild on file changes** (install `cargo-watch` first with `cargo install cargo-watch --locked`):
```sh
cargo watch -x 'build --bin borg' -x 'build --bin borgd'
```

## Project Structure

Borg is a Cargo workspace. See the Architecture table in [AGENTS.md](AGENTS.md#architecture) for the crate list and what each one owns.

## Running Tests

```sh
cargo test                         # all tests
cargo test -p coding     # patch DSL tests
cargo test -p cortex            # core tests
cargo test -p gateway         # gateway tests
cargo test -p plugins         # plugin tests
```

## Code Style

- Run `cargo fmt` before committing
- Run `cargo clippy -- -D warnings` and fix any warnings
- The lefthook `pre-commit` hook runs both in parallel; `pre-push` runs the full test suite
- `just complexity` reports functions with cyclomatic complexity > 15 (lizard via `uvx`); keep new code under that bar

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

- **Bugs:** Use the [bug report template](https://github.com/adelphidata/borg/issues/new?template=bug_report.md)
- **Features:** Use the [feature request template](https://github.com/adelphidata/borg/issues/new?template=feature_request.md)
- **Security:** See [SECURITY.md](SECURITY.md) — do not open public issues for vulnerabilities

## Questions?

Open a [discussion](https://github.com/adelphidata/borg/discussions).
